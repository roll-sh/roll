//! The Security plugin's REST API (`_plugins/_security/…`, and `_opendistro/_security/…`):
//! `api/internalusers`, `api/roles`, `api/rolesmapping`, `api/actiongroups` and `api/tenants`
//! (GET, PUT, DELETE, PATCH), `api/account`, `api/permissionsinfo`, `api/securityconfig` (GET;
//! changing it is refused, as `allow_securityconfig_modification` is off by default),
//! `api/audit` (GET, PUT, PATCH: stored, roll writes no audit log), `api/ssl` and `whoami` (as a
//! node without HTTP TLS answers: no certificates for anyone without an admin certificate),
//! `api/cache` (DELETE: forgets verified passwords), `authinfo` and `health`.
//!
//! Answers are the plugin's: `{"status": "CREATED", "message": "'x' created."}`, validation
//! errors as `{"status": "error", "reason": …}`, entries printed with their `reserved`, `static`
//! and `hidden` flags and every key sorted. The configuration API is open to the owner, to
//! users with a role of `plugins.security.restapi.roles_enabled` (`all_access`,
//! `security_rest_api_access`), and to users whose roles grant `restapi:admin/<type>`.

use super::{Identity, Kind, entries, entry, flag, has_default, patch, principal, strs, tombstone};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::{Api, Method};
use crate::xpack::store;

/// Whether this module handles an API.
pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::PluginsSecurityInternalusers
            | Api::PluginsSecurityRoles
            | Api::PluginsSecurityRolesmapping
            | Api::PluginsSecurityActiongroups
            | Api::PluginsSecurityAccount
            | Api::PluginsSecurityAuthinfo
            | Api::PluginsSecurityHealth
            | Api::PluginsSecurityTenants
            | Api::PluginsSecurityPermissionsinfo
            | Api::PluginsSecurityWhoami
            | Api::PluginsSecuritySecurityconfig
            | Api::PluginsSecurityAudit
            | Api::PluginsSecuritySsl
            | Api::PluginsSecurityCache
    )
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let kind = match ctx.api {
        Api::PluginsSecurityInternalusers => Kind::Users,
        Api::PluginsSecurityRoles => Kind::Roles,
        Api::PluginsSecurityRolesmapping => Kind::Mappings,
        Api::PluginsSecurityActiongroups => Kind::Groups,
        Api::PluginsSecurityAccount => return account(ctx).await,
        Api::PluginsSecurityAuthinfo => return Ok(authinfo(ctx)),
        Api::PluginsSecurityHealth => return Reply::ok(health()),
        Api::PluginsSecurityTenants => Kind::Tenants,
        Api::PluginsSecurityPermissionsinfo => return Ok(permissionsinfo(ctx)),
        // `WhoAmIAction` reads the request's TLS certificate, which plain HTTP has none of.
        Api::PluginsSecurityWhoami => return Ok(Reply::Raw(403, "text/plain; charset=UTF-8".into(), bytes::Bytes::from_static(b"No security data"))),
        Api::PluginsSecuritySecurityconfig => {
            return Ok(match access(ctx, None) {
                Some(denied) => denied,
                None if ctx.method == Method::Get => Reply::Body(200, obj! { "config" => settings("securityconfig") }),
                None => message(403, "FORBIDDEN", "Access denied"),
            });
        }
        Api::PluginsSecurityAudit => return audit(ctx).await,
        // The certificates and their reload are the super admin's (an admin TLS certificate).
        Api::PluginsSecuritySsl => return Ok(access(ctx, None).unwrap_or_else(|| message(403, "FORBIDDEN", "Access denied"))),
        Api::PluginsSecurityCache => {
            if let Some(denied) = access(ctx, None) {
                return Ok(denied);
            }
            ctx.cluster.xpack.verified.lock().clear();
            return Ok(message(200, "OK", "Cache flushed successfully."));
        }
        other => return Err(EsError::not_implemented(other.name())),
    };
    if let Some(denied) = access(ctx, Some(kind.endpoint())) {
        return Ok(denied);
    }
    let name = ctx.path_param("name").filter(|n| !n.is_empty()).map(str::to_string);
    match (ctx.method, name) {
        (Method::Get, name) => Ok(get(ctx, kind, name.as_deref())),
        (Method::Put, Some(name)) => put(ctx, kind, &name).await,
        (Method::Delete, Some(name)) => delete(ctx, kind, &name).await,
        (Method::Patch, name) => patch_entries(ctx, kind, name.as_deref()).await,
        _ => Ok(message(400, "BAD_REQUEST", "No name given")),
    }
}

// ------------------------------------------------------------------------------ answers

/// `{"status": <word>, "message": …}`.
fn message(status: u16, word: &str, msg: impl Into<String>) -> Reply {
    Reply::Body(status, obj! { "status" => word, "message" => msg.into() })
}

/// A validation error: `{"status": "error", "reason": …, <details>}`.
fn invalid(reason: &str, details: Vec<(String, Value)>) -> Reply {
    let mut m = Map::new();
    m.insert("status".into(), "error".into());
    m.insert("reason".into(), reason.into());
    m.extend(details);
    Reply::Body(400, Value::Obj(m))
}

fn not_found(kind: Kind, name: &str) -> Reply {
    message(404, "NOT_FOUND", format!("{} '{name}' not found.", kind.noun()))
}

/// The 403 of a static or reserved entry (`None`: neither).
fn protected(v: &Value, name: &str) -> Option<Reply> {
    let what = if flag(v, "static") {
        "static"
    } else if flag(v, "reserved") {
        "reserved"
    } else {
        return None;
    };
    Some(message(403, "FORBIDDEN", format!("Resource '{name}' is {what}.")))
}

/// The configuration API's access check (`RestApiAdminPrivilegesEvaluator`): the owner, a role
/// of `roles_enabled`, or a role that grants `restapi:admin/<endpoint>` explicitly
/// (`hasExplicitClusterPrivilege`: a role whose cluster permissions are `*` doesn't count; the
/// endpoints without such a permission (`None`) only open to the first two).
fn access(ctx: &Ctx, endpoint: Option<&str>) -> Option<Reply> {
    let p = principal(ctx);
    if p.os().is_some_and(|id| id.owner) || p.roles.iter().any(|r| r == "all_access" || r == "security_rest_api_access") {
        return None;
    }
    let refused = || {
        message(
            403,
            "FORBIDDEN",
            format!(
                "No permission to access REST API: User {} with Security roles [{}] does not have any role privileged for admin access. No ssl info found in request.",
                p.username,
                p.roles.join(", ")
            ),
        )
    };
    let Some(endpoint) = endpoint else { return Some(refused()) };
    let st = ctx.state();
    let mut explicit = p.clone();
    explicit.roles.retain(|r| entry(&st, Kind::Roles, r).is_some_and(|role| !super::expand(&st, &strs(role.get("cluster_permissions"))).iter().any(|a| a == "*:*")));
    let eff = super::super::effective(&super::super::file_roles(ctx.cluster), &st, &explicit);
    if eff.allows_cluster(&format!("restapi:admin/{endpoint}")) {
        return None;
    }
    Some(refused())
}

/// The flags each type prints.
fn flags(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Mappings => &["hidden", "reserved"],
        _ => &["hidden", "reserved", "static"],
    }
}

/// An entry as `GET` prints it: its flags, no hash, every key sorted.
pub fn view(kind: Kind, v: &Value) -> Value {
    let mut m = v.as_object().cloned().unwrap_or_default();
    for f in flags(kind) {
        let on = flag(v, f);
        m.insert((*f).into(), on.into());
    }
    if kind == Kind::Users {
        m.insert("hash".into(), "".into());
    }
    sort(&mut m);
    for key in ["index_permissions", "tenant_permissions"] {
        if let Some(Value::Arr(a)) = m.get_mut(key) {
            for e in a {
                if let Value::Obj(em) = e {
                    sort(em);
                }
            }
        }
    }
    Value::Obj(m)
}

fn sort(m: &mut Map) {
    m.sort_keys();
}

/// An entry without its flags (and an internal user without its hash): what PATCH edits.
fn editable(kind: Kind, v: &Value) -> Value {
    let mut m = v.as_object().cloned().unwrap_or_default();
    for f in ["hidden", "reserved", "static"] {
        m.shift_remove(f);
    }
    if kind == Kind::Users {
        m.shift_remove("hash");
    }
    Value::Obj(m)
}

// ------------------------------------------------------------------------------ GET

fn get(ctx: &Ctx, kind: Kind, name: Option<&str>) -> Reply {
    let st = ctx.state();
    let mut out = Map::new();
    match name {
        Some(n) => match entry(&st, kind, n).filter(|v| !flag(v, "hidden")) {
            Some(v) => {
                out.insert(n.to_string(), view(kind, &v));
            }
            None => return not_found(kind, n),
        },
        None => {
            let mut all: Vec<(String, Value)> = entries(&st, kind).into_iter().filter(|(_, v)| !flag(v, "hidden")).collect();
            // Tenants list in the order of the plugin's `HashMap`.
            if kind == Kind::Tenants {
                let order = crate::mapping::java_hash_map_order(all.iter().map(|(n, _)| n.as_str()));
                all = order.into_iter().map(|i| all[i].clone()).collect();
            }
            for (n, v) in all {
                out.insert(n, view(kind, &v));
            }
        }
    }
    Reply::Body(200, Value::Obj(out))
}

// ------------------------------------------------------------------------------ validation

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    Arr,
    Str,
    Obj,
    Bool,
}

/// What a type's body may hold (`RequestContentValidator.ValidationContext`).
struct Schema {
    keys: &'static [(&'static str, Ty)],
    mandatory: &'static [&'static str],
    one_of: &'static [&'static str],
}

fn schema(kind: Kind) -> Schema {
    match kind {
        Kind::Users => Schema {
            keys: &[("attributes", Ty::Obj), ("backend_roles", Ty::Arr), ("description", Ty::Str), ("opendistro_security_roles", Ty::Arr), ("password", Ty::Str), ("hash", Ty::Str)],
            mandatory: &[],
            one_of: &[],
        },
        Kind::Roles => Schema {
            keys: &[("cluster_permissions", Ty::Arr), ("index_permissions", Ty::Arr), ("tenant_permissions", Ty::Arr), ("description", Ty::Str)],
            mandatory: &[],
            one_of: &[],
        },
        Kind::Mappings => Schema {
            keys: &[("backend_roles", Ty::Arr), ("and_backend_roles", Ty::Arr), ("hosts", Ty::Arr), ("users", Ty::Arr), ("description", Ty::Str)],
            mandatory: &[],
            one_of: &["backend_roles", "hosts", "and_backend_roles", "users"],
        },
        Kind::Groups => Schema { keys: &[("allowed_actions", Ty::Arr), ("type", Ty::Str), ("description", Ty::Str)], mandatory: &["allowed_actions"], one_of: &[] },
        Kind::Tenants => Schema { keys: &[("description", Ty::Str)], mandatory: &[], one_of: &[] },
    }
}

const ACCOUNT: Schema = Schema { keys: &[("current_password", Ty::Str), ("password", Ty::Str), ("hash", Ty::Str)], mandatory: &["current_password"], one_of: &[] };

fn required() -> Reply {
    invalid("Request body required for this action.", Vec::new())
}

/// The request body as an object, `Err` with the plugin's answer otherwise.
fn body_object(ctx: &Ctx) -> Result<Map, Reply> {
    if ctx.body.iter().all(u8::is_ascii_whitespace) {
        return Err(required());
    }
    match crate::json::parse(&ctx.body) {
        Ok(Some(Value::Obj(m))) if !m.is_empty() => Ok(m),
        Ok(_) => Err(required()),
        Err(_) => Err(invalid("Could not parse content of request.", Vec::new())),
    }
}

/// Checks a body's keys and their types; then a password's strength (`user`: whose).
fn validate(m: &Map, schema: &Schema, user: Option<&str>) -> Result<(), Reply> {
    let keys = |ks: Vec<&str>| obj! { "keys" => ks.join(",") };
    let unknown: Vec<&str> = m.keys().map(String::as_str).filter(|k| !schema.keys.iter().any(|(n, _)| n == k)).collect();
    let missing: Vec<&str> = schema.mandatory.iter().copied().filter(|k| !m.contains_key(*k)).collect();
    let none_of = !schema.one_of.is_empty() && !schema.one_of.iter().any(|k| m.contains_key(*k));
    if !unknown.is_empty() || !missing.is_empty() || none_of {
        let mut details = Vec::new();
        if !unknown.is_empty() {
            details.push(("invalid_keys".to_string(), keys(unknown)));
        }
        if !missing.is_empty() {
            details.push(("missing_mandatory_keys".to_string(), keys(missing)));
        }
        if none_of {
            details.push(("specify_one_of".to_string(), keys(schema.one_of.to_vec())));
        }
        return Err(invalid("Invalid configuration", details));
    }
    let mut wrong = Vec::new();
    for (k, v) in m {
        let Some((_, ty)) = schema.keys.iter().find(|(n, _)| n == k) else { continue };
        let (ok, expected) = match ty {
            Ty::Arr => (matches!(v, Value::Arr(_)), "Array expected"),
            Ty::Str => (matches!(v, Value::Str(_)), "String expected"),
            Ty::Obj => (matches!(v, Value::Obj(_)), "Object expected"),
            Ty::Bool => (matches!(v, Value::Bool(_)), "Boolean expected"),
        };
        if !ok {
            wrong.push((k.clone(), Value::from(expected)));
        }
    }
    if !wrong.is_empty() {
        return Err(invalid("Wrong datatype", wrong));
    }
    if let (Some(user), Some(pw)) = (user, m.get("password").and_then(Value::as_str))
        && let Some(why) = super::password::check(user, pw)
    {
        return Err(invalid(why, Vec::new()));
    }
    Ok(())
}

const V7: &str = "org.opensearch.security.securityconf.impl.v7";

/// The class the plugin reads a type's entries into (named in its parse errors).
fn class(kind: Kind) -> &'static str {
    match kind {
        Kind::Users => "InternalUserV7",
        Kind::Roles => "RoleV7",
        Kind::Mappings => "RoleMappingsV7",
        Kind::Groups => "ActionGroupsV7",
        Kind::Tenants => "TenantV7",
    }
}

/// The plugin's 500 for a body its config classes can't read (Jackson's message).
fn jackson(exception: &str, kind_name: &'static str, msg: String) -> EsError {
    let reason = format!("tools.jackson.databind.exc.{exception}: {msg}");
    EsError::new(500, "i_o_exception", reason.clone()).caused_by(EsError::new(500, kind_name, msg)).with_root_causes(vec![EsError::new(500, "i_o_exception", reason)])
}

/// A string list that holds an object or a list (numbers and booleans read as strings).
fn check_strings(v: &Value, chain: &str) -> EsResult<()> {
    for (i, e) in v.as_array().into_iter().flatten().enumerate() {
        let (what, token) = match e {
            Value::Obj(_) => ("Object", "START_OBJECT"),
            Value::Arr(_) => ("Array", "START_ARRAY"),
            _ => continue,
        };
        return Err(jackson(
            "MismatchedInputException",
            "mismatched_input_exception",
            format!(
                "Cannot deserialize value of type `java.lang.String` from {what} value (token `JsonToken.{token}`)\n at [No location information] (through reference chain: {chain}->java.util.ArrayList[{i}])"
            ),
        ));
    }
    Ok(())
}

/// The nested entries of a role (`index_permissions`, `tenant_permissions`), read as the
/// plugin's `RoleV7$Index` and `RoleV7$Tenant`.
fn check_shapes(kind: Kind, m: &Map) -> EsResult<()> {
    let top = class(kind);
    for (k, v) in m {
        if matches!(v, Value::Arr(_)) && k != "index_permissions" && k != "tenant_permissions" {
            check_strings(v, &format!("{V7}.{top}[\"{k}\"]"))?;
        }
    }
    if kind != Kind::Roles {
        return Ok(());
    }
    for (field, inner, known) in [
        ("index_permissions", "Index", &["allowed_actions", "dls", "fls", "index_patterns", "masked_fields"][..]),
        ("tenant_permissions", "Tenant", &["allowed_actions", "tenant_patterns"][..]),
    ] {
        for (i, e) in m.get(field).and_then(Value::as_array).into_iter().flatten().enumerate() {
            let Some(em) = e.as_object() else {
                let node = match e {
                    Value::Str(_) => "StringNode",
                    Value::Num(_) => "IntNode",
                    Value::Bool(_) => "BooleanNode",
                    Value::Arr(_) => "ArrayNode",
                    _ => "NullNode",
                };
                return Err(EsError::new(500, "json_node_exception", format!("`JsonNode` not of type `ObjectNode` (but `tools.jackson.databind.node.{node})`, cannot call `withArray()` on it")));
            };
            for (ek, ev) in em {
                let chain = format!("{V7}.RoleV7[\"{field}\"]->java.util.ArrayList[{i}]->{V7}.RoleV7${inner}[\"{ek}\"]");
                if !known.contains(&ek.as_str()) {
                    let props = known.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", ");
                    return Err(jackson(
                        "UnrecognizedPropertyException",
                        "unrecognized_property_exception",
                        format!(
                            "Unrecognized property \"{ek}\" (class {V7}.RoleV7${inner}), not marked as ignorable ({} known properties: {props})\n at [No location information] (through reference chain: {chain})",
                            known.len()
                        ),
                    ));
                }
                if ek != "dls" {
                    check_strings(ev, &chain)?;
                }
            }
        }
    }
    Ok(())
}

/// A validated body as the entry stored: every list and object the type has, in its shape.
fn normalize(kind: Kind, m: &Map) -> Value {
    let list = |k: &str| Value::Arr(strs(m.get(k)).into_iter().map(Value::from).collect());
    let mut out = Map::new();
    let keys: &[&str] = match kind {
        Kind::Users => &["backend_roles", "opendistro_security_roles"],
        Kind::Roles => &["cluster_permissions"],
        Kind::Mappings => &["and_backend_roles", "backend_roles", "hosts", "users"],
        Kind::Groups => &["allowed_actions"],
        Kind::Tenants => &[],
    };
    for k in keys {
        out.insert((*k).into(), list(k));
    }
    if let Some(d) = m.get("description").and_then(Value::as_str) {
        out.insert("description".into(), d.into());
    }
    match kind {
        Kind::Users => {
            out.insert("attributes".into(), m.get("attributes").cloned().unwrap_or_else(Value::object));
        }
        Kind::Roles => {
            let entries = |field: &str, lists: &[&str], dls: bool| -> Value {
                let mut out = Vec::new();
                for e in m.get(field).and_then(Value::as_array).into_iter().flatten() {
                    let mut em = Map::new();
                    for k in lists {
                        em.insert((*k).into(), Value::Arr(strs(e.get(k)).into_iter().map(Value::from).collect()));
                    }
                    if dls && let Some(q) = e.get("dls").filter(|q| !q.is_null()) {
                        em.insert("dls".into(), Value::from(q.to_es_string()));
                    }
                    out.push(Value::Obj(em));
                }
                Value::Arr(out)
            };
            out.insert("index_permissions".into(), entries("index_permissions", &["allowed_actions", "fls", "index_patterns", "masked_fields"], true));
            out.insert("tenant_permissions".into(), entries("tenant_permissions", &["allowed_actions", "tenant_patterns"], false));
        }
        Kind::Groups => {
            if let Some(t) = m.get("type").and_then(Value::as_str) {
                out.insert("type".into(), t.into());
            }
        }
        Kind::Mappings | Kind::Tenants => {}
    }
    let mut out = Value::Obj(out);
    if let Value::Obj(o) = &mut out {
        sort(o);
    }
    out
}

/// An entry a PUT or PATCH makes from `m` (validated): the type's own checks against the
/// cluster's configuration, and the internal user's hash.
fn make(st: &crate::meta::ClusterState, kind: Kind, name: &str, m: &Map, existing: Option<&Value>) -> Result<Value, Reply> {
    let visible = |k: Kind, n: &str| entry(st, k, n).is_some_and(|v| !flag(&v, "hidden"));
    let mut v = normalize(kind, m);
    match kind {
        Kind::Users => {
            if name.contains(':') {
                return Err(message(400, "BAD_REQUEST", "A restricted character(s) was detected in the account name. Please remove: ':'"));
            }
            for r in strs(m.get("opendistro_security_roles")) {
                if !visible(Kind::Roles, &r) {
                    return Err(not_found(Kind::Roles, &r));
                }
            }
            let hash = match (m.get("password").and_then(Value::as_str), m.get("hash").and_then(Value::as_str)) {
                (Some(pw), _) => crate::xpack::security::hash_password(pw),
                (None, Some(h)) => h.to_string(),
                (None, None) => match existing.map(|e| e.get("hash").and_then(Value::as_str).unwrap_or_default()) {
                    Some(h) if !h.is_empty() => h.to_string(),
                    Some(_) => return Err(message(400, "BAD_REQUEST", format!("Existing user {name} has no password, and no new password or hash was specified."))),
                    None => return Err(message(400, "BAD_REQUEST", "Please specify either 'hash' or 'password' when creating a new internal user.")),
                },
            };
            if let Value::Obj(o) = &mut v {
                o.insert("hash".into(), hash.into());
                sort(o);
            }
        }
        Kind::Mappings => {
            if !visible(Kind::Roles, name) {
                return Err(not_found(Kind::Roles, name));
            }
        }
        Kind::Groups => {
            if let Some(t) = m.get("type").and_then(Value::as_str)
                && !matches!(t, "cluster" | "index")
            {
                return Err(message(400, "BAD_REQUEST", format!("Invalid action group type: {t}. Supported types are: cluster, index.")));
            }
            if strs(m.get("allowed_actions")).iter().any(|a| a == name) {
                return Err(message(400, "BAD_REQUEST", format!("{name} cannot be an allowed_action of itself")));
            }
        }
        Kind::Tenants => {}
        Kind::Roles => {
            // Only the plugin's super admin (a TLS admin certificate, which roll has none of)
            // may grant the REST API's own permissions.
            if strs(m.get("cluster_permissions")).iter().any(|c| c.starts_with("restapi:")) {
                return Err(message(403, "FORBIDDEN", "Access denied"));
            }
        }
    }
    Ok(v)
}

// ------------------------------------------------------------------------------ PUT, DELETE

async fn put(ctx: &Ctx<'_>, kind: Kind, name: &str) -> EsResult<Reply> {
    let m = match body_object(ctx) {
        Ok(m) => m,
        Err(r) => return Ok(r),
    };
    if let Err(r) = validate(&m, &schema(kind), (kind == Kind::Users).then_some(name)) {
        return Ok(r);
    }
    check_shapes(kind, &m)?;
    let st = ctx.state();
    let existing = entry(&st, kind, name);
    if let Some(r) = existing.as_ref().and_then(|e| protected(e, name)) {
        return Ok(r);
    }
    let v = match make(&st, kind, name, &m, existing.as_ref()) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let (family, key, now) = (kind.family(), name.to_string(), ctx.now_ms);
    let created = ctx
        .cluster
        .update(move |st| {
            let created = entry(st, kind, &key).is_none();
            store::put(st, family, &key, v, now)?;
            Ok(created)
        })
        .await?;
    Ok(if created { message(201, "CREATED", format!("'{name}' created.")) } else { message(200, "OK", format!("'{name}' updated.")) })
}

async fn delete(ctx: &Ctx<'_>, kind: Kind, name: &str) -> EsResult<Reply> {
    let st = ctx.state();
    let Some(existing) = entry(&st, kind, name).filter(|v| !flag(v, "hidden")) else { return Ok(not_found(kind, name)) };
    if let Some(r) = protected(&existing, name) {
        return Ok(r);
    }
    let (key, now) = (name.to_string(), ctx.now_ms);
    ctx.cluster.update(move |st| remove(st, kind, &key, now)).await?;
    Ok(message(200, "OK", format!("'{name}' deleted.")))
}

/// Removes an entry (a default one stays hidden by a tombstone).
fn remove(st: &mut crate::meta::ClusterState, kind: Kind, name: &str, now: i64) -> EsResult<()> {
    if has_default(kind, name) {
        store::put(st, kind.family(), name, tombstone(), now)?;
    } else {
        store::remove(st, kind.family(), name);
    }
    Ok(())
}

// ------------------------------------------------------------------------------ PATCH

async fn patch_entries(ctx: &Ctx<'_>, kind: Kind, name: Option<&str>) -> EsResult<Reply> {
    let body = match crate::json::parse(&ctx.body) {
        Ok(Some(v)) => v,
        _ => return Ok(message(400, "BAD_REQUEST", "Wrong request body")),
    };
    let st = ctx.state();
    // The document the patch edits: one entry, or every visible entry by name.
    let before: Vec<(String, Value)> = match name {
        Some(n) => match entry(&st, kind, n).filter(|v| !flag(v, "hidden")) {
            Some(v) => {
                if let Some(r) = protected(&v, n) {
                    return Ok(r);
                }
                vec![(n.to_string(), v)]
            }
            None => return Ok(not_found(kind, n)),
        },
        None => entries(&st, kind).into_iter().filter(|(_, v)| !flag(v, "hidden")).collect(),
    };
    let original: Map = before.iter().map(|(n, v)| (n.clone(), editable(kind, v))).collect();
    let mut doc = match name {
        Some(n) => original.get(n).cloned().unwrap_or_else(Value::object),
        None => Value::Obj(original.clone()),
    };
    match patch::apply(&mut doc, &body) {
        Ok(()) => {}
        Err(patch::PatchError::Body) => return Ok(message(400, "BAD_REQUEST", "Wrong request body")),
        Err(patch::PatchError::Missing(f)) => return Ok(message(400, "BAD_REQUEST", format!("Missing field \"{f}\""))),
        Err(patch::PatchError::Invalid(m)) => return Ok(message(400, "BAD_REQUEST", m)),
    }
    let after: Map = match (name, doc) {
        (Some(n), v) => std::iter::once((n.to_string(), v)).collect(),
        (None, Value::Obj(m)) => m,
        (None, _) => return Ok(message(400, "BAD_REQUEST", "Wrong request body")),
    };
    let existing: std::collections::HashMap<&str, &Value> = before.iter().map(|(n, v)| (n.as_str(), v)).collect();
    let mut writes: Vec<(String, Option<Value>)> = Vec::new();
    for (n, v) in &after {
        if original.get(n) == Some(v) {
            continue;
        }
        let Some(m) = v.as_object() else { return Ok(message(400, "BAD_REQUEST", "Wrong request body")) };
        if let Some(r) = existing.get(n.as_str()).and_then(|e| protected(e, n)) {
            return Ok(r);
        }
        if m.is_empty() {
            return Ok(required());
        }
        if let Err(r) = validate(m, &schema(kind), (kind == Kind::Users).then_some(n.as_str())) {
            return Ok(r);
        }
        check_shapes(kind, m)?;
        match make(&st, kind, n, m, existing.get(n.as_str()).copied()) {
            Ok(v) => writes.push((n.clone(), Some(v))),
            Err(r) => return Ok(r),
        }
    }
    for (n, v) in &before {
        if !after.contains_key(n) {
            if let Some(r) = protected(v, n) {
                return Ok(r);
            }
            writes.push((n.clone(), None));
        }
    }
    if writes.is_empty() {
        return Ok(message(200, "OK", "No updates required"));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            for (n, v) in writes {
                match v {
                    Some(v) => {
                        store::put(st, kind.family(), &n, v, now)?;
                    }
                    None => remove(st, kind, &n, now)?,
                }
            }
            Ok(())
        })
        .await?;
    Ok(match name {
        Some(n) => message(200, "OK", format!("'{n}' updated.")),
        None => message(200, "OK", "Resource updated."),
    })
}

// ------------------------------------------------------------------------------ account, authinfo, health

/// The tenants a user may use, `true` where it may write: the tenants its roles'
/// `tenant_permissions` name, then its private tenant.
fn tenants(ctx: &Ctx, p: &crate::xpack::security::Principal) -> Value {
    let st = ctx.state();
    let mut out = Map::new();
    let mut all: Vec<String> = entries(&st, Kind::Tenants).into_iter().filter(|(_, v)| !flag(v, "hidden")).map(|(n, _)| n).collect();
    // The plugin's own order: the global tenant first.
    all.sort_by_key(|t| (t != "global_tenant", t.clone()));
    for t in &all {
        let t = t.as_str();
        let mut access: Option<bool> = None;
        for r in p.roles.iter().filter_map(|r| entry(&st, Kind::Roles, r)) {
            for tp in r.get("tenant_permissions").and_then(Value::as_array).into_iter().flatten() {
                if !strs(tp.get("tenant_patterns")).iter().any(|pat| super::wildcard(pat, t)) {
                    continue;
                }
                let write = super::expand(&st, &strs(tp.get("allowed_actions"))).iter().any(|a| a.ends_with("/write") || a == "*:*");
                access = Some(access.unwrap_or(false) || write);
            }
        }
        if let Some(w) = access {
            out.insert(t.into(), w.into());
        }
    }
    out.insert(p.username.clone(), true.into());
    Value::Obj(out)
}

fn attribute_names(id: &Identity) -> Value {
    Value::Arr(id.attributes.keys().map(|k| Value::from(format!("attr.internal.{k}"))).collect())
}

fn strings(v: &[String]) -> Value {
    Value::Arr(v.iter().map(Value::from).collect())
}

fn authinfo(ctx: &Ctx) -> Reply {
    let p = principal(ctx);
    let id = p.os().cloned().unwrap_or_default();
    let remote = ctx.request.header("x-forwarded-for").and_then(|h| h.split(',').next()).map(|h| h.trim().to_string()).unwrap_or_else(|| "127.0.0.1:0".into());
    let mut m = Map::new();
    m.insert("user".into(), super::user_string(&p, &id).into());
    m.insert("user_name".into(), p.username.clone().into());
    m.insert("user_requested_tenant".into(), Value::Null);
    m.insert("remote_address".into(), remote.into());
    m.insert("backend_roles".into(), strings(&id.backend_roles));
    m.insert("custom_attribute_names".into(), attribute_names(&id));
    m.insert("roles".into(), strings(&p.roles));
    m.insert("tenants".into(), tenants(ctx, &p));
    m.insert("principal".into(), Value::Null);
    m.insert("peer_certificates".into(), "0".into());
    m.insert("sso_logout_url".into(), Value::Null);
    Reply::Body(200, Value::Obj(m))
}

fn health() -> Value {
    obj! { "message" => Value::Null, "mode" => "strict", "status" => "UP", "settings" => obj! { "plugins.security.cache.ttl_minutes" => 60 } }
}

async fn account(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let p = principal(ctx);
    let id = p.os().cloned().unwrap_or_default();
    if ctx.method != Method::Put {
        let mut m = Map::new();
        m.insert("user_name".into(), p.username.clone().into());
        m.insert("is_reserved".into(), id.reserved.into());
        m.insert("is_hidden".into(), false.into());
        m.insert("is_internal_user".into(), true.into());
        m.insert("user_requested_tenant".into(), Value::Null);
        m.insert("backend_roles".into(), strings(&id.backend_roles));
        m.insert("custom_attribute_names".into(), attribute_names(&id));
        m.insert("tenants".into(), tenants(ctx, &p));
        m.insert("roles".into(), strings(&p.roles));
        return Ok(Reply::Body(200, Value::Obj(m)));
    }
    let m = match body_object(ctx) {
        Ok(m) => m,
        Err(r) => return Ok(r),
    };
    if let Err(r) = validate(&m, &ACCOUNT, Some(&p.username)) {
        return Ok(r);
    }
    let st = ctx.state();
    let name = p.username.clone();
    let Some(user) = entry(&st, Kind::Users, &name) else { return Ok(not_found(Kind::Users, &name)) };
    if let Some(r) = protected(&user, &name) {
        return Ok(r);
    }
    let current = m.get("current_password").and_then(Value::as_str).unwrap_or_default();
    let stored = user.get("hash").and_then(Value::as_str).unwrap_or_default();
    if stored.is_empty() || !super::verify_hash(current, stored) {
        return Ok(message(400, "BAD_REQUEST", "Could not validate your current password."));
    }
    let hash = match (m.get("password").and_then(Value::as_str).filter(|s| !s.is_empty()), m.get("hash").and_then(Value::as_str).filter(|s| !s.is_empty())) {
        (Some(pw), _) => crate::xpack::security::hash_password(pw),
        (None, Some(h)) => h.to_string(),
        (None, None) => return Ok(message(400, "BAD_REQUEST", "Both provided password and hash cannot be null/empty.")),
    };
    let (key, now) = (name.clone(), ctx.now_ms);
    ctx.cluster
        .update(move |st| {
            let Some(Value::Obj(mut u)) = entry(st, Kind::Users, &key) else { return Ok(()) };
            u.insert("hash".into(), hash.into());
            store::put(st, Kind::Users.family(), &key, Value::Obj(u), now).map(|_| ())
        })
        .await?;
    Ok(message(200, "OK", format!("'{name}' updated.")))
}

// ------------------------------------------------------------------------------ permissionsinfo

/// The REST API's endpoints (`Endpoint`), in the order the plugin prints them.
const ENDPOINTS: &[&str] = &[
    "AUDIT", "INTERNALUSERS", "ROLES", "ALLOWLIST", "TENANTS", "VALIDATE", "APITOKENS", "VIEW_VERSION", "ROLESMAPPING", "RESOURCE_SHARING", "NODESDN", "PERMISSIONSINFO",
    "SSL", "ACTIONGROUPS", "RATELIMITERS", "CONFIG", "ACCOUNT", "CACHE", "SYSTEMINFO", "MIGRATE", "ROLLBACK_VERSION", "AUTHTOKEN",
];

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE", "CONNECT"];

/// `PermissionsInfoAction`: whether the user may use the REST API, and the endpoints it may not
/// (every method of every endpoint, when it may use none).
fn permissionsinfo(ctx: &Ctx) -> Reply {
    let p = principal(ctx);
    let id = p.os().cloned().unwrap_or_default();
    let api = access(ctx, None).is_none();
    let mut disabled = Map::new();
    if !api {
        for e in ENDPOINTS {
            disabled.insert((*e).into(), Value::Arr(METHODS.iter().map(|m| Value::from(*m)).collect()));
        }
    }
    Reply::Body(200, obj! { "user" => super::user_string(&p, &id), "user_name" => p.username.clone(), "has_api_access" => api, "disabled_endpoints" => Value::Obj(disabled) })
}

// ------------------------------------------------------------------------------ securityconfig, audit

/// The plugin's default configuration documents (`settings.json`): `securityconfig` (its demo
/// `config.yml`), `audit` (`audit.yml`) and `audit_defaults` (`AuditConfig`'s own defaults, what
/// a `PUT` starts from).
fn settings(name: &str) -> Value {
    static S: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let all = S.get_or_init(|| crate::json::parse(include_bytes!("settings.json")).ok().flatten().unwrap_or(Value::Null));
    all.get(name).cloned().unwrap_or(Value::Null)
}

const AUDIT: &str = "opendistro_security.audit";

/// `AuditCategory`.
const CATEGORIES: &[&str] = &[
    "BAD_HEADERS", "FAILED_LOGIN", "MISSING_PRIVILEGES", "GRANTED_PRIVILEGES", "OPENDISTRO_SECURITY_INDEX_ATTEMPT", "SSL_EXCEPTION", "AUTHENTICATED", "INDEX_EVENT",
    "COMPLIANCE_DOC_READ", "COMPLIANCE_DOC_WRITE", "COMPLIANCE_EXTERNAL_CONFIG", "COMPLIANCE_INTERNAL_CONFIG_READ", "COMPLIANCE_INTERNAL_CONFIG_WRITE",
    "CLUSTER_SETTINGS_CHANGED", "INDEX_SETTINGS_CHANGED", "RESOURCE_ACCESS_GRANTED", "RESOURCE_ACCESS_DENIED", "RESOURCE_SHARING_CHANGED",
];

#[derive(Clone, Copy)]
enum Field {
    Bool,
    Strings,
    Categories,
    /// `read_watched_fields`: index pattern to field names.
    Watched,
}

const AUDIT_FIELDS: &[(&str, Field)] = &[
    ("disabled_categories", Field::Categories),
    ("disabled_rest_categories", Field::Categories),
    ("disabled_transport_categories", Field::Categories),
    ("enable_rest", Field::Bool),
    ("enable_transport", Field::Bool),
    ("exclude_sensitive_headers", Field::Bool),
    ("ignore_headers", Field::Strings),
    ("ignore_requests", Field::Strings),
    ("ignore_url_params", Field::Strings),
    ("ignore_users", Field::Strings),
    ("log_request_body", Field::Bool),
    ("resolve_bulk_requests", Field::Bool),
    ("resolve_indices", Field::Bool),
];

const COMPLIANCE_FIELDS: &[(&str, Field)] = &[
    ("enabled", Field::Bool),
    ("external_config", Field::Bool),
    ("internal_config", Field::Bool),
    ("read_ignore_users", Field::Strings),
    ("read_metadata_only", Field::Bool),
    ("read_watched_fields", Field::Watched),
    ("write_ignore_users", Field::Strings),
    ("write_log_diffs", Field::Bool),
    ("write_metadata_only", Field::Bool),
    ("write_watched_indices", Field::Strings),
];

const AUDIT_SCHEMA: Schema = Schema { keys: &[("enabled", Ty::Bool), ("audit", Ty::Obj), ("compliance", Ty::Obj)], mandatory: &[], one_of: &[] };

/// An audit configuration from a validated body: `AuditConfig`'s defaults with the body's values
/// (`None`: a section the config class can't read, which the plugin answers as unparsable).
fn audit_config(m: &Map) -> Option<Value> {
    let strings = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(|e| matches!(e, Value::Str(_))));
    let ok = |f: Field, v: &Value| match f {
        Field::Bool => matches!(v, Value::Bool(_)),
        Field::Strings => strings(v),
        Field::Categories => strings(v) && v.as_array().into_iter().flatten().all(|c| c.as_str().is_some_and(|c| CATEGORIES.contains(&c))),
        Field::Watched => v.as_object().is_some_and(|w| w.values().all(strings)),
    };
    let mut cfg = settings("audit_defaults");
    if let Some(e) = m.get("enabled") {
        cfg.as_object_mut()?.insert("enabled".into(), e.clone());
    }
    for (section, fields) in [("audit", AUDIT_FIELDS), ("compliance", COMPLIANCE_FIELDS)] {
        let Some(given) = m.get(section).and_then(Value::as_object) else { continue };
        let target = cfg.as_object_mut()?.get_mut(section)?.as_object_mut()?;
        for (k, v) in given {
            let (_, f) = fields.iter().find(|(n, _)| n == k)?;
            if !ok(*f, v) {
                return None;
            }
            target.insert(k.clone(), v.clone());
        }
    }
    Some(cfg)
}

fn current_audit(st: &crate::meta::ClusterState) -> Value {
    st.metadata.xpack.get(AUDIT, "config").cloned().unwrap_or_else(|| settings("audit"))
}

async fn audit(ctx: &Ctx<'_>) -> EsResult<Reply> {
    if let Some(denied) = access(ctx, None) {
        return Ok(denied);
    }
    let st = ctx.state();
    let cfg = match ctx.method {
        Method::Get => return Ok(Reply::Body(200, obj! { "_readonly" => Value::Arr(Vec::new()), "config" => current_audit(&st) })),
        Method::Put => {
            let m = match body_object(ctx) {
                Ok(m) => m,
                Err(r) => return Ok(r),
            };
            if let Err(r) = validate(&m, &AUDIT_SCHEMA, None) {
                return Ok(r);
            }
            match audit_config(&m) {
                Some(c) => c,
                None => return Ok(invalid("Could not parse content of request.", Vec::new())),
            }
        }
        _ => {
            let body = match crate::json::parse(&ctx.body) {
                Ok(Some(v)) => v,
                _ => return Ok(message(400, "BAD_REQUEST", "Wrong request body")),
            };
            let before = obj! { "config" => current_audit(&st) };
            let mut doc = before.clone();
            match patch::apply(&mut doc, &body) {
                Ok(()) => {}
                Err(patch::PatchError::Body) => return Ok(message(400, "BAD_REQUEST", "Wrong request body")),
                Err(patch::PatchError::Missing(f)) => return Ok(message(400, "BAD_REQUEST", format!("Missing field \"{f}\""))),
                Err(patch::PatchError::Invalid(m)) => return Ok(message(400, "BAD_REQUEST", m)),
            }
            if doc == before {
                return Ok(message(200, "OK", "No updates required"));
            }
            let parsed = doc.get("config").and_then(Value::as_object).filter(|m| validate(m, &AUDIT_SCHEMA, None).is_ok()).and_then(audit_config);
            match parsed {
                Some(c) => c,
                None => return Ok(invalid("Could not parse content of request.", Vec::new())),
            }
        }
    };
    let patched = ctx.method == Method::Patch;
    let now = ctx.now_ms;
    ctx.cluster.update(move |st| store::put(st, AUDIT, "config", cfg, now).map(|_| ())).await?;
    Ok(if patched { message(200, "OK", "Resource updated.") } else { message(200, "OK", "'config' updated.") })
}
