//! API keys: create, grant, cross-cluster keys, get, query, update, invalidate, and
//! authentication by `Authorization: ApiKey base64(id:secret)`.
//!
//! A key is stored with its owner, its own role descriptors and a snapshot of its owner's role
//! descriptors at creation (`limited_by`): its permissions are the intersection, as in
//! Elasticsearch. The secret (16 random bytes) is kept only as a salted SHA-256.

use super::query::{self, QueryRequest};
use super::{AuthKind, Principal};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::TimeValue;
use crate::xpack::store;

pub const KEYS: &str = "security.api_key";

/// Authenticates the value of an `ApiKey` header.
pub fn authenticate(cluster: &Cluster, value: &str) -> EsResult<Principal> {
    let decoded = super::b64_decode(value).map_err(|e| super::unauthorized_credentials(format!("API key: {e}")))?;
    let text = String::from_utf8_lossy(&decoded).into_owned();
    let Some((id, secret)) = text.split_once(':').filter(|(i, s)| !i.is_empty() && !s.is_empty()) else {
        return Err(super::unauthorized_credentials("API key: invalid ApiKey value"));
    };
    let st = cluster.state();
    let Some(k) = st.metadata.xpack.get(KEYS, id) else {
        return Err(super::unauthorized_credentials(format!("API key: unable to find apikey with id {id}")));
    };
    if k.get("invalidated").and_then(Value::as_bool) == Some(true) {
        return Err(super::unauthorized_credentials(format!("API key: api key [{id}] has been invalidated")));
    }
    if let Some(exp) = k.get("expiration").and_then(Value::as_i64)
        && exp <= crate::time::now_millis()
    {
        return Err(super::unauthorized_credentials("API key: api key is expired"));
    }
    let hash = k.get("hash").and_then(Value::as_str).unwrap_or_default();
    if !super::verify_secret(secret, hash) {
        return Err(super::unauthorized_credentials(format!("API key: invalid credentials for API key [{id}]")));
    }
    // A cross-cluster key only connects remote clusters, never REST clients. (Elasticsearch
    // checks the type before the secret; checking it after tells nothing to a caller that
    // only knows the id.)
    if let Some(ty) = k.get("type").and_then(Value::as_str).filter(|t| *t != "rest") {
        return Err(super::unauthorized(format!("authentication expected API key type of [rest], but API key [{id}] has type [{ty}]")));
    }
    Ok(Principal {
        username: k.get("username").map(Value::to_es_string).unwrap_or_default(),
        roles: Vec::new(),
        full_name: k.get("owner_full_name").cloned().unwrap_or(Value::Null),
        email: k.get("owner_email").cloned().unwrap_or(Value::Null),
        metadata: k.get("owner_metadata").cloned().unwrap_or_else(Value::object),
        realm_name: "_es_api_key",
        realm_type: "_es_api_key",
        authenticating: None,
        kind: AuthKind::ApiKey(Box::new(super::ApiKeyAuth {
            id: id.to_string(),
            name: k.get("name").cloned().unwrap_or(Value::Null),
            role_descriptors: k.get("role_descriptors").cloned().unwrap_or_else(Value::object),
            limited_by: k.get("limited_by").cloned().unwrap_or_else(Value::object),
            cross_cluster: k.get("type").and_then(Value::as_str) == Some("cross_cluster"),
        })),
    })
}

/// The owner's role descriptors (`limited_by`) for a key `p` creates.
fn limited_by(file: &super::FileRoles, st: &crate::meta::ClusterState, p: &Principal) -> Value {
    match &p.kind {
        // A key created with an API key is limited by that key's own limits.
        AuthKind::ApiKey(a) => a.limited_by.clone(),
        _ => {
            let mut m = Map::new();
            for r in &p.roles {
                if let Some(d) = super::resolve_role(file, st, r) {
                    m.insert(r.clone(), d);
                }
            }
            Value::Obj(m)
        }
    }
}

fn parse_descriptors(v: Option<&Value>) -> EsResult<Value> {
    let mut out = Map::new();
    if let Some(Value::Obj(m)) = v {
        for (name, d) in m {
            out.insert(name.clone(), super::users::parse_role(name, d, true)?);
        }
    }
    Ok(Value::Obj(out))
}

fn expiration_of(v: Option<&Value>, now: i64) -> EsResult<Option<i64>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(x) => Ok(Some(now + TimeValue::parse(&x.to_es_string(), "expiration")?.as_millis())),
    }
}

fn check_name(name: &Value) -> EsResult<()> {
    let s = name.as_str().unwrap_or_default();
    if s.is_empty() {
        return Err(EsError::action_request_validation(&["api key name is required".into()]));
    }
    if s.chars().count() > 256 {
        return Err(EsError::action_request_validation(&["api key name may not be more than 256 characters long".into()]));
    }
    if s != s.trim() {
        return Err(EsError::action_request_validation(&["api key name may not begin or end with whitespace".into()]));
    }
    if s.starts_with('_') {
        return Err(EsError::action_request_validation(&["api key name may not begin with an underscore".into()]));
    }
    Ok(())
}

struct NewKey {
    name: Value,
    expiration: Option<i64>,
    role_descriptors: Value,
    metadata: Value,
    cross_cluster: Option<Value>,
}

async fn store_key(ctx: &Ctx<'_>, owner: &Principal, k: NewKey) -> EsResult<Reply> {
    let id = crate::meta::names::time_based_id();
    let secret = super::random_b64url(16);
    let st = ctx.state();
    let limited = if k.cross_cluster.is_some() { Value::object() } else { limited_by(&super::file_roles(ctx.cluster), &st, owner) };
    let (realm, realm_type) = match &owner.kind {
        AuthKind::ApiKey(_) => ("_es_api_key", "_es_api_key"),
        _ => (owner.realm_name, owner.realm_type),
    };
    let mut doc = Map::new();
    doc.insert("id".into(), id.clone().into());
    doc.insert("name".into(), k.name.clone());
    doc.insert("type".into(), (if k.cross_cluster.is_some() { "cross_cluster" } else { "rest" }).into());
    doc.insert("creation".into(), ctx.now_ms.into());
    if let Some(e) = k.expiration {
        doc.insert("expiration".into(), e.into());
    }
    doc.insert("invalidated".into(), false.into());
    doc.insert("username".into(), owner.username.clone().into());
    doc.insert("realm".into(), realm.into());
    doc.insert("realm_type".into(), realm_type.into());
    doc.insert("metadata".into(), k.metadata);
    doc.insert("role_descriptors".into(), k.role_descriptors);
    doc.insert("limited_by".into(), limited);
    if let Some(a) = k.cross_cluster {
        doc.insert("access".into(), a);
    }
    doc.insert("owner_metadata".into(), owner.metadata.clone());
    doc.insert("owner_full_name".into(), owner.full_name.clone());
    doc.insert("owner_email".into(), owner.email.clone());
    doc.insert("hash".into(), super::hash_secret(&secret).into());
    let now = ctx.now_ms;
    let key_id = id.clone();
    ctx.cluster.update(move |st| store::put(st, KEYS, &key_id, Value::Obj(doc), now)).await?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let mut out = Map::new();
    out.insert("id".into(), id.into());
    out.insert("name".into(), k.name);
    if let Some(e) = k.expiration {
        out.insert("expiration".into(), e.into());
    }
    out.insert("api_key".into(), secret.into());
    out.insert("encoded".into(), encoded.into());
    Reply::ok(Value::Obj(out))
}

/// `CreateApiKeyRequestBuilder.PARSER` (`api_key_request`): its parse errors, then the
/// request validation.
fn parse_create(ctx: &Ctx, body: &Value) -> EsResult<NewKey> {
    use crate::json::locate::{Seg, Target, locate};
    const PARSER: &str = "api_key_request";
    if let Some(m) = body.as_object()
        && let Some(k) = m.keys().find(|k| !matches!(k.as_str(), "name" | "expiration" | "role_descriptors" | "metadata"))
    {
        return Err(crate::xpack::unknown_field(ctx, PARSER, k));
    }
    let name = match body.get("name") {
        None => return Err(EsError::illegal_argument("Required [name]")),
        Some(Value::Null) => {
            let (l, c) = crate::xpack::value_loc(ctx, "name");
            return Err(EsError::x_content_parse(l, c, format!("[{PARSER}] name doesn't support values of type: VALUE_NULL")));
        }
        Some(n) => n.clone(),
    };
    let expiration = expiration_of(body.get("expiration"), ctx.now_ms)
        .map_err(|e| EsError::x_content_parse_bare(format!("Failed to build [{PARSER}] after last required field arrived")).caused_by(e))?;
    let role_descriptors = match body.get("role_descriptors") {
        Some(Value::Arr(_)) => {
            let (l, c) = crate::xpack::value_loc(ctx, "role_descriptors");
            return Err(EsError::x_content_parse(l, c, format!("[{PARSER}] failed to parse field [role_descriptors]"))
                .caused_by(EsError::illegal_argument("[role_descriptors] doesn't support arrays. Use a single object with multiple fields.")));
        }
        Some(Value::Obj(m)) => {
            let mut out = Map::new();
            for (rname, d) in m {
                match super::users::parse_role(rname, d, true) {
                    Ok(r) => {
                        out.insert(rname.clone(), r);
                    }
                    Err(e) => {
                        // Where the role parser stood: the offending field's value.
                        let bad = e.reason().rsplit_once("unexpected field [").map(|(_, f)| f.trim_end_matches(']').to_string());
                        let (l, c) = ctx
                            .raw_json()
                            .and_then(|src| match &bad {
                                Some(f) => locate(src, &[Seg::Key("role_descriptors"), Seg::Key(rname), Seg::Key(f)], Target::Value),
                                None => locate(src, &[Seg::Key("role_descriptors"), Seg::Key(rname)], Target::Value),
                            })
                            .unwrap_or((1, 1));
                        return Err(EsError::x_content_parse(l, c, format!("[{PARSER}] failed to parse field [role_descriptors]"))
                            .caused_by(EsError::x_content_parse(l, c, format!("[role_descriptors] failed to parse field [{rname}]")).caused_by(e)));
                    }
                }
            }
            Value::Obj(out)
        }
        _ => Value::object(),
    };
    let metadata = body.get("metadata").cloned().unwrap_or_else(Value::object);
    let mut errors: Vec<String> = Vec::new();
    if let Err(e) = check_name(&name) {
        errors.push(e.reason().trim_start_matches("Validation Failed: 1: ").trim_end_matches(';').to_string());
    }
    if metadata.as_object().is_some_and(|m| m.keys().any(|k| k.starts_with('_'))) {
        errors.push("API key metadata keys may not start with [_]".into());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    Ok(NewKey { name, expiration, role_descriptors, metadata, cross_cluster: None })
}

pub async fn create(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let k = parse_create(ctx, &body)?;
    let owner = super::principal(ctx).clone();
    store_key(ctx, &owner, k).await
}

/// `access` of a cross-cluster key as Elasticsearch keeps it (`CrossClusterApiKeyRoleDescriptorBuilder`):
/// the normalized access, and the role descriptor it grants: search reads (with their document
/// and field level security) under `cross_cluster_search` and `monitor_enrich`, replication
/// under `cross_cluster_replication`.
fn cross_cluster_descriptor(access: &Value) -> EsResult<(Value, Value)> {
    let Some(m) = access.as_object() else { return Err(EsError::illegal_argument("must specify non-empty access for either [search] or [replication]")) };
    let (mut cluster, mut indices, mut out) = (Vec::new(), Vec::new(), Map::new());
    let entries = |k: &str| m.get(k).and_then(Value::as_array).filter(|a| !a.is_empty());
    let (search, replication) = (entries("search"), entries("replication"));
    if let Some(s) = search {
        if replication.is_some() && s.iter().any(|e| e.get("query").is_some() || e.get("field_security").is_some()) {
            return Err(EsError::new(400, "x_content_parse_exception", "Failed to build [cross_cluster_api_key_request] after last required field arrived")
                .caused_by(EsError::illegal_argument("search does not support document or field level security if replication is assigned")));
        }
        cluster.extend([Value::from("cross_cluster_search"), Value::from("monitor_enrich")]);
        let mut shown = Vec::new();
        for e in s {
            let mut a = Map::new();
            a.insert("names".into(), e.get("names").cloned().unwrap_or_else(|| Value::Arr(Vec::new())));
            if let Some(fs) = e.get("field_security") {
                a.insert("field_security".into(), fs.clone());
            }
            if let Some(q) = e.get("query") {
                a.insert("query".into(), Value::from(q.as_str().map(str::to_string).unwrap_or_else(|| q.to_json())));
            }
            a.insert("allow_restricted_indices".into(), e.get("allow_restricted_indices").and_then(Value::as_bool).unwrap_or(false).into());
            let mut g = a.clone();
            g.shift_insert(1, "privileges".into(), crate::arr!["read", "read_cross_cluster", "view_index_metadata"]);
            indices.push(Value::Obj(g));
            shown.push(Value::Obj(a));
        }
        out.insert("search".into(), Value::Arr(shown));
    }
    if let Some(r) = replication {
        cluster.push(Value::from("cross_cluster_replication"));
        let mut shown = Vec::new();
        for e in r {
            let names = e.get("names").cloned().unwrap_or_else(|| Value::Arr(Vec::new()));
            let restricted = e.get("allow_restricted_indices").and_then(Value::as_bool).unwrap_or(false);
            indices.push(obj! {
                "names" => names.clone(),
                "privileges" => crate::arr!["cross_cluster_replication", "cross_cluster_replication_internal"],
                "allow_restricted_indices" => restricted,
            });
            shown.push(obj! { "names" => names, "allow_restricted_indices" => restricted });
        }
        out.insert("replication".into(), Value::Arr(shown));
    }
    if cluster.is_empty() {
        return Err(EsError::illegal_argument("must specify non-empty access for either [search] or [replication]"));
    }
    let descriptor = obj! { "cross_cluster" => obj! {
        "cluster" => Value::Arr(cluster), "indices" => Value::Arr(indices), "applications" => Value::Arr(Vec::new()),
        "run_as" => Value::Arr(Vec::new()), "metadata" => obj! {}, "transient_metadata" => obj! { "enabled" => true },
    } };
    Ok((Value::Obj(out), descriptor))
}

pub async fn create_cross_cluster(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let name = body.get("name").cloned().unwrap_or(Value::Null);
    check_name(&name)?;
    let (access, role_descriptors) = cross_cluster_descriptor(body.get("access").unwrap_or(&Value::Null))?;
    let k = NewKey {
        name,
        expiration: expiration_of(body.get("expiration"), ctx.now_ms)?,
        role_descriptors,
        metadata: body.get("metadata").cloned().unwrap_or_else(Value::object),
        cross_cluster: Some(access),
    };
    let owner = super::principal(ctx).clone();
    store_key(ctx, &owner, k).await
}

pub async fn grant(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let grant_type = body.get("grant_type").and_then(Value::as_str);
    if body.get("client_authentication").is_some() && grant_type == Some("password") {
        return Err(EsError::action_request_validation(&["[client_authentication] is not supported for grant_type [password]".into()]));
    }
    let mut owner = super::tokens::grant_principal(ctx.cluster, &body, "cluster:admin/xpack/security/api_key/grant")?;
    // Client authentication goes with JWTs, which roll doesn't issue: no access token of roll's
    // takes it.
    if body.get("client_authentication").is_some() {
        return Err(EsError::security(400, "[client_authentication] not supported with the supplied access_token type"));
    }
    // `run_as`: the key belongs to the user the granted credentials may run as.
    if let Some(target) = body.get("run_as").and_then(Value::as_str) {
        let st = ctx.state();
        let eff = super::effective(&super::file_roles(ctx.cluster), &st, &owner);
        match super::users::lookup_user(ctx.cluster, target).filter(|_| eff.allows_run_as(target)) {
            Some(u) => owner = u,
            None => return Err(super::run_as_denied(&owner, "cluster:admin/xpack/security/user/authenticate", target)),
        }
    }
    let key = body.get("api_key").cloned().unwrap_or_else(Value::object);
    let name = key.get("name").cloned().unwrap_or(Value::Null);
    check_name(&name)?;
    let k = NewKey {
        name,
        expiration: expiration_of(key.get("expiration"), ctx.now_ms)?,
        role_descriptors: parse_descriptors(key.get("role_descriptors"))?,
        metadata: key.get("metadata").cloned().unwrap_or_else(Value::object),
        cross_cluster: None,
    };
    store_key(ctx, &owner, k).await
}

/// Whether `manage_own_api_key` grants the request (`ManageOwnApiKeyClusterPrivilege`):
/// creating, querying and updating keys (their handlers act on the caller's own keys only),
/// and getting or invalidating keys the request shows to be the caller's own: `owner=true`,
/// or the caller's username with its realm; an API key only itself, by its id. Granting keys
/// and every other API key request need `manage_api_key`.
pub fn own_keys_request(ctx: &Ctx, p: &Principal) -> bool {
    let owns = |id: Option<&str>, username: Option<&str>, realm: Option<&str>, owner: bool| match &p.kind {
        AuthKind::ApiKey(a) => id == Some(a.id.as_str()),
        _ => owner || matches!((username, realm), (Some(u), Some(r)) if !u.is_empty() && !r.is_empty() && u == p.username && r == p.realm_name),
    };
    // A key sees its owner's role descriptors only with `manage_api_key`.
    if matches!(p.kind, AuthKind::ApiKey(_)) && ctx.bool_param("with_limited_by", false).unwrap_or(true) {
        return false;
    }
    match ctx.api.name() {
        "security.create_api_key" | "security.query_api_keys" | "security.update_api_key" | "security.bulk_update_api_keys" => true,
        "security.get_api_key" => owns(ctx.query("id"), ctx.query("username"), ctx.query("realm_name"), ctx.bool_param("owner", false).unwrap_or(false)),
        "security.invalidate_api_key" => {
            let Ok(body) = ctx.body() else { return false };
            let body = body.unwrap_or_else(Value::object);
            let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
            let (username, realm) = (text("username"), text("realm_name"));
            let owner = body.get("owner").and_then(Value::as_bool).unwrap_or(false);
            let mut ids: Vec<String> = body.get("ids").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
            ids.extend(text("id"));
            if ids.is_empty() {
                owns(None, username.as_deref(), realm.as_deref(), owner)
            } else {
                ids.iter().all(|i| owns(Some(i), username.as_deref(), realm.as_deref(), owner))
            }
        }
        _ => false,
    }
}

/// Whether the principal may see and manage every key (`manage_api_key`), not only its own.
fn manages_all(ctx: &Ctx) -> bool {
    let p = super::principal(ctx);
    p.is_superuser() || super::effective(&super::file_roles(ctx.cluster), &ctx.state(), p).has_cluster_privilege("manage_api_key")
}

/// Whether `k` is the principal's own: a user's keys, or an API key itself (it owns no other).
fn owned_by(k: &Value, p: &Principal) -> bool {
    match &p.kind {
        AuthKind::ApiKey(a) => k.get("id").and_then(Value::as_str) == Some(a.id.as_str()),
        _ => k.get("username").and_then(Value::as_str) == Some(p.username.as_str()) && k.get("realm").and_then(Value::as_str) == Some(p.realm_name),
    }
}

/// A key as the get and query APIs print it.
fn key_view(k: &Value, with_limited_by: bool, profile_uid: Option<Value>) -> Value {
    let mut out = Map::new();
    for f in ["id", "name", "type", "creation", "expiration", "invalidated", "invalidation", "username", "realm", "realm_type", "metadata", "role_descriptors"] {
        if let Some(v) = k.get(f) {
            out.insert(f.into(), v.clone());
        }
    }
    if with_limited_by && k.get("type").and_then(Value::as_str) != Some("cross_cluster") {
        out.insert("limited_by".into(), Value::Arr(vec![k.get("limited_by").cloned().unwrap_or_else(Value::object)]));
    }
    if let Some(a) = k.get("access") {
        out.insert("access".into(), a.clone());
    }
    if let Some(u) = profile_uid {
        out.insert("profile_uid".into(), u);
    }
    Value::Obj(out)
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let p = super::principal(ctx);
    let all = manages_all(ctx);
    let owner = ctx.bool_param("owner", false)?;
    let active_only = ctx.bool_param("active_only", false)?;
    let with_limited_by = ctx.bool_param("with_limited_by", false)?;
    let id = ctx.query("id");
    let name = ctx.query("name");
    let username = ctx.query("username");
    let realm = ctx.query("realm_name");
    if (id.is_some() || name.is_some()) && (username.is_some() || realm.is_some()) && owner {
        return Err(EsError::action_request_validation(&["username or realm name must not be specified when the current user owner flag is set".into()]));
    }
    let mut keys = Vec::new();
    for (_, k) in st.metadata.xpack.iter(KEYS) {
        if (!all || owner) && !owned_by(k, p) {
            continue;
        }
        if id.is_some_and(|i| k.get("id").and_then(Value::as_str) != Some(i)) {
            continue;
        }
        if let Some(n) = name
            && !crate::xcontent::filter::glob_match(n, k.get("name").and_then(Value::as_str).unwrap_or_default())
        {
            continue;
        }
        if username.is_some_and(|u| k.get("username").and_then(Value::as_str) != Some(u)) || realm.is_some_and(|r| k.get("realm").and_then(Value::as_str) != Some(r)) {
            continue;
        }
        if active_only && (k.get("invalidated").and_then(Value::as_bool) == Some(true) || k.get("expiration").and_then(Value::as_i64).is_some_and(|e| e <= ctx.now_ms)) {
            continue;
        }
        keys.push((k.get("creation").and_then(Value::as_i64).unwrap_or(0), key_view(k, with_limited_by, None)));
    }
    keys.sort_by_key(|(c, _)| *c);
    if keys.is_empty() && (id.is_some() || name.is_some()) {
        return Err(EsError::resource_not_found(match (id, name) {
            (Some(i), _) => format!("api key with id [{i}] was not found"),
            (_, Some(n)) => format!("api key with name [{n}] was not found"),
            _ => "api key was not found".into(),
        }));
    }
    Reply::ok(obj! { "api_keys" => Value::Arr(keys.into_iter().map(|(_, v)| v).collect()) })
}

fn key_fields(k: &Value, f: &str) -> Vec<Value> {
    match f {
        "id" | "_id" => query::path_values(k, "id"),
        "name" | "type" | "creation" | "expiration" | "invalidated" | "invalidation" | "username" => query::path_values(k, f),
        "realm_name" | "realm" => query::path_values(k, "realm"),
        _ if f.starts_with("metadata.") || f.starts_with("metadata_flattened") => query::path_values(k, &f.replacen("metadata_flattened", "metadata", 1)),
        "*" => query::path_values(k, "*"),
        _ => Vec::new(),
    }
}

pub fn query_keys(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?;
    let req = QueryRequest::parse(body.as_ref(), "query_api_key_request")?;
    let st = ctx.state();
    let p = super::principal(ctx);
    let all = manages_all(ctx);
    let with_limited_by = ctx.bool_param("with_limited_by", false)?;
    let objs: Vec<(String, Value)> = st.metadata.xpack.iter(KEYS).filter(|(_, k)| all || owned_by(k, p)).map(|(i, k)| (i.clone(), k.clone())).collect();
    let (total, page) = query::run(&req, objs, &key_fields)?;
    let sorted = !req.sort.is_empty();
    let hits = page.into_iter().map(|(k, keys)| query::with_sort(key_view(&k, with_limited_by, None), keys, sorted)).collect();
    Reply::ok(query::response(total, "api_keys", hits))
}

pub async fn invalidate(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let mut ids: Vec<String> = match body.get("ids") {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        _ => Vec::new(),
    };
    if let Some(i) = body.get("id") {
        ids.push(i.to_es_string());
    }
    let name = body.get("name").map(Value::to_es_string);
    let username = body.get("username").map(Value::to_es_string);
    let realm = body.get("realm_name").map(Value::to_es_string);
    let owner = body.get("owner").and_then(Value::as_bool).unwrap_or(false);
    if ids.is_empty() && name.is_none() && username.is_none() && realm.is_none() && !owner {
        return Err(EsError::action_request_validation(&["One of [api key id(s), api key name, username, realm name] must be specified if [owner] flag is false".into()]));
    }
    let p = super::principal(ctx).clone();
    let all = manages_all(ctx) && !owner;
    let now = ctx.now_ms;
    let invalidated = ctx
        .cluster
        .update(move |st| {
            let mut out = Vec::new();
            let candidates: Vec<(String, Value)> = st.metadata.xpack.iter(KEYS).map(|(i, k)| (i.clone(), k.clone())).collect();
            for (id, mut k) in candidates {
                if k.get("invalidated").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if !all && !owned_by(&k, &p) {
                    continue;
                }
                if !ids.is_empty() && !ids.contains(&id) {
                    continue;
                }
                if name.as_ref().is_some_and(|n| !crate::xcontent::filter::glob_match(n, k.get("name").and_then(Value::as_str).unwrap_or_default())) {
                    continue;
                }
                if username.as_ref().is_some_and(|u| k.get("username").and_then(Value::as_str) != Some(u.as_str()))
                    || realm.as_ref().is_some_and(|r| k.get("realm").and_then(Value::as_str) != Some(r.as_str()))
                {
                    continue;
                }
                if let Value::Obj(m) = &mut k {
                    m.insert("invalidated".into(), true.into());
                    m.insert("invalidation".into(), now.into());
                }
                store::put(st, KEYS, &id, k, now)?;
                out.push(id);
            }
            Ok(out)
        })
        .await?;
    Reply::ok(obj! {
        "invalidated_api_keys" => Value::Arr(invalidated.into_iter().map(Value::from).collect()),
        "previously_invalidated_api_keys" => Value::Arr(Vec::new()),
        "error_count" => 0,
    })
}

/// Applies an update to one key; `Ok(true)` when it changed anything.
fn apply_update(st: &mut crate::meta::ClusterState, file: &super::FileRoles, id: &str, p: &Principal, upd: &Update, cross: bool, now: i64) -> EsResult<bool> {
    let not_found = || EsError::resource_not_found(format!("no API key owned by requesting user found for ID [{id}]"));
    let Some(mut k) = st.metadata.xpack.get(KEYS, id).cloned() else { return Err(not_found()) };
    if !owned_by(&k, p) || k.get("invalidated").and_then(Value::as_bool) == Some(true) {
        return Err(not_found());
    }
    let is_cross = k.get("type").and_then(Value::as_str) == Some("cross_cluster");
    if is_cross != cross {
        return Err(EsError::illegal_argument(if cross {
            "cannot update API key of type [rest] while expected type is [cross_cluster]"
        } else {
            "cannot update API key of type [cross_cluster] while expected type is [rest]"
        }));
    }
    let before = k.clone();
    let limited = if cross { Value::object() } else { limited_by(file, st, p) };
    if let Value::Obj(m) = &mut k {
        if let Some(rd) = &upd.role_descriptors {
            m.insert("role_descriptors".into(), rd.clone());
        }
        if let Some(a) = &upd.access {
            m.insert("access".into(), a.clone());
        }
        if let Some(md) = &upd.metadata {
            m.insert("metadata".into(), md.clone());
        }
        if let Some(e) = upd.expiration {
            m.insert("expiration".into(), e.into());
        }
        m.insert("limited_by".into(), limited);
    }
    if k == before {
        return Ok(false);
    }
    store::put(st, KEYS, id, k, now)?;
    Ok(true)
}

#[derive(Clone)]
struct Update {
    role_descriptors: Option<Value>,
    access: Option<Value>,
    metadata: Option<Value>,
    expiration: Option<i64>,
}

fn parse_update(body: &Value, now: i64, cross: bool) -> EsResult<Update> {
    let (access, role_descriptors) = match (body.get("access"), cross) {
        (Some(a), true) => {
            let (a, d) = cross_cluster_descriptor(a)?;
            (Some(a), Some(d))
        }
        (_, false) if body.get("role_descriptors").is_some() => (None, Some(parse_descriptors(body.get("role_descriptors"))?)),
        _ => (None, None),
    };
    Ok(Update {
        role_descriptors,
        access,
        metadata: body.get("metadata").cloned(),
        expiration: expiration_of(body.get("expiration"), now)?,
    })
}

/// Who updates keys: a user, never an API key (`TransportBaseUpdateApiKeyAction`).
fn updater(ctx: &Ctx) -> EsResult<Principal> {
    let p = super::principal(ctx);
    if matches!(p.kind, AuthKind::ApiKey(_)) {
        return Err(EsError::illegal_argument("authentication via API key not supported: only the owner user can update an API key"));
    }
    Ok(p.clone())
}

pub async fn update(ctx: &Ctx<'_>, cross: bool) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let upd = parse_update(&body, ctx.now_ms, cross)?;
    let p = updater(ctx)?;
    let now = ctx.now_ms;
    let file = super::file_roles(ctx.cluster);
    let updated = ctx.cluster.update(move |st| apply_update(st, &file, &id, &p, &upd, cross, now)).await?;
    Reply::ok(obj! { "updated" => updated })
}

pub async fn bulk_update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let ids: Vec<String> = body.get("ids").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    let upd = parse_update(&body, ctx.now_ms, false)?;
    let p = updater(ctx)?;
    let now = ctx.now_ms;
    let file = super::file_roles(ctx.cluster);
    let results = ctx
        .cluster
        .update(move |st| {
            Ok(ids
                .iter()
                .map(|id| (id.clone(), apply_update(st, &file, id, &p, &upd, false, now)))
                .collect::<Vec<_>>())
        })
        .await?;
    let (mut updated, mut noops, mut errors) = (Vec::new(), Vec::new(), Map::new());
    for (id, r) in results {
        match r {
            Ok(true) => updated.push(Value::from(id)),
            Ok(false) => noops.push(Value::from(id)),
            Err(e) => {
                errors.insert(id, obj! { "type" => e.kind.to_string(), "reason" => e.reason().to_string() });
            }
        }
    }
    let mut out = Map::new();
    out.insert("updated".into(), Value::Arr(updated));
    out.insert("noops".into(), Value::Arr(noops));
    if !errors.is_empty() {
        out.insert("errors".into(), obj! { "count" => errors.len(), "details" => Value::Obj(errors) });
    }
    Reply::ok(Value::Obj(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_cluster_access_becomes_a_role() {
        let (_, d) = cross_cluster_descriptor(&obj! { "search" => crate::arr![obj! { "names" => crate::arr!["logs*"] }] }).unwrap();
        assert_eq!(d.pointer(&["cross_cluster", "cluster"]), Some(&crate::arr!["cross_cluster_search", "monitor_enrich"]));
        assert!(cross_cluster_descriptor(&obj! {}).is_err());
    }

    #[test]
    fn names_are_validated() {
        assert!(check_name(&Value::from("k")).is_ok());
        assert!(check_name(&Value::Null).is_err());
        assert!(check_name(&Value::from("_k")).is_err());
    }
}
