//! OAuth2 tokens (`_security/oauth2/token`), service accounts and their tokens, user profiles,
//! and the small security APIs (`_authenticate`, `_security/stats`, `_security/settings`).
//!
//! Access and refresh tokens are 29 random bytes (base64, 40 characters like Elasticsearch's),
//! found again by their SHA-256; they expire after 20 minutes (`expires_in: 1200`). Service
//! account tokens use Elasticsearch's format (`AAEAA…`: a version header, then
//! `namespace/service/name:secret`), the secret kept as a salted SHA-256.

use super::{AuthKind, Principal};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub const TOKENS: &str = "security.token";
pub const SERVICE_TOKENS: &str = "security.service_token";
pub const PROFILES: &str = "security.profile";

const TOKEN_LIFETIME_MS: i64 = 20 * 60 * 1000;

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_key(token: &str) -> String {
    hex(&super::digest(&[token.as_bytes()]))
}

fn new_token() -> String {
    use base64::Engine;
    let bytes: [u8; 29] = rand::random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn token_expired() -> EsError {
    EsError::security(401, "token expired").with_header("WWW-Authenticate", "Bearer realm=\"security\", error=\"invalid_token\", error_description=\"The access token expired\"")
}

/// Authenticates a `Bearer` value: an access token or a service account token.
pub fn authenticate_bearer(cluster: &Cluster, value: &str) -> EsResult<Principal> {
    if value.starts_with("AAEAA") {
        return authenticate_service_token(cluster, value);
    }
    let st = cluster.state();
    let Some(t) = st.metadata.xpack.get(TOKENS, &token_key(value)) else {
        return Err(super::unauthorized_credentials("oauth2 token: invalid token"));
    };
    let now = crate::time::now_millis();
    if t.get("invalidated").and_then(Value::as_bool) == Some(true) || t.get("expires").and_then(Value::as_i64).is_some_and(|e| e <= now) {
        return Err(token_expired());
    }
    let mut p = principal_from_user(t.get("user").unwrap_or(&Value::Null));
    p.kind = AuthKind::Token;
    Ok(p)
}

fn realm_of(name: &str) -> (&'static str, &'static str) {
    match name {
        "reserved" => ("reserved", "reserved"),
        "default_file" => ("default_file", "file"),
        "_es_api_key" => ("_es_api_key", "_es_api_key"),
        _ => ("default_native", "native"),
    }
}

fn principal_from_user(u: &Value) -> Principal {
    let (realm_name, realm_type) = realm_of(u.get("realm").and_then(Value::as_str).unwrap_or("default_native"));
    Principal {
        username: u.get("username").map(Value::to_es_string).unwrap_or_default(),
        roles: u.get("roles").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default(),
        full_name: u.get("full_name").cloned().unwrap_or(Value::Null),
        email: u.get("email").cloned().unwrap_or(Value::Null),
        metadata: u.get("metadata").cloned().unwrap_or_else(Value::object),
        realm_name,
        realm_type,
        authenticating: None,
        kind: AuthKind::Realm,
    }
}

fn stored_user(p: &Principal) -> Value {
    obj! {
        "username" => p.username.clone(),
        "roles" => Value::Arr(p.roles.iter().map(Value::from).collect()),
        "full_name" => p.full_name.clone(),
        "email" => p.email.clone(),
        "metadata" => p.metadata.clone(),
        "realm" => p.realm_name,
    }
}

/// The user a grant (`grant_type: password | access_token`) names, authenticated.
pub fn grant_principal(cluster: &Cluster, body: &Value, action: &str) -> EsResult<Principal> {
    match body.get("grant_type").and_then(Value::as_str) {
        Some("password") => {
            let user = body.get("username").map(Value::to_es_string).unwrap_or_default();
            let pass = body.get("password").map(Value::to_es_string).unwrap_or_default();
            if user.is_empty() || pass.is_empty() {
                return Err(EsError::action_request_validation(&[if user.is_empty() { "username is missing".into() } else { "password is missing".into() }]));
            }
            super::users::authenticate_password(cluster, &user, &pass).ok_or_else(|| super::unauthorized(format!("unable to authenticate user [{user}] for action [{action}]")))
        }
        Some("access_token") => {
            let t = body.get("access_token").map(Value::to_es_string).unwrap_or_default();
            authenticate_bearer(cluster, &t)
        }
        Some(other) => Err(EsError::action_request_validation(&[format!("grant_type [{other}] is not supported")])),
        None => Err(EsError::action_request_validation(&["[grant_type] is required".into()])),
    }
}

async fn issue(ctx: &Ctx<'_>, p: &Principal, refresh: bool, authentication: Value) -> EsResult<Reply> {
    let access = new_token();
    let refresh_token = refresh.then(new_token);
    let now = ctx.now_ms;
    let doc = obj! {
        "user" => stored_user(p),
        "created" => now,
        "expires" => now + TOKEN_LIFETIME_MS,
        "invalidated" => false,
        "refresh" => refresh_token.as_ref().map(|r| Value::from(token_key(r))).unwrap_or(Value::Null),
        "refresh_used" => false,
    };
    let key = token_key(&access);
    ctx.cluster.update(move |st| store::put(st, TOKENS, &key, doc, now).map(|_| ())).await?;
    let mut out = Map::new();
    out.insert("access_token".into(), access.into());
    out.insert("type".into(), "Bearer".into());
    out.insert("expires_in".into(), (TOKEN_LIFETIME_MS / 1000).into());
    if let Some(r) = refresh_token {
        out.insert("refresh_token".into(), r.into());
    }
    out.insert("authentication".into(), authentication);
    Reply::ok(Value::Obj(out))
}

pub async fn get_token(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    const ACTION: &str = "cluster:admin/xpack/security/token/create";
    match body.get("grant_type").and_then(Value::as_str) {
        Some("password") => {
            let p = grant_principal(ctx.cluster, &body, ACTION)?;
            let auth = p.to_value();
            issue(ctx, &p, true, auth).await
        }
        Some("client_credentials") => {
            let p = super::principal(ctx).clone();
            let auth = p.to_value();
            issue(ctx, &p, false, auth).await
        }
        Some("refresh_token") => {
            let r = body.get("refresh_token").map(Value::to_es_string).unwrap_or_default();
            let rk = token_key(&r);
            let now = ctx.now_ms;
            let user = ctx
                .cluster
                .update(move |st| {
                    let found = st.metadata.xpack.iter(TOKENS).find(|(_, t)| t.get("refresh").and_then(Value::as_str) == Some(rk.as_str())).map(|(k, t)| (k.clone(), t.clone()));
                    let Some((k, mut t)) = found else {
                        return Err(EsError::new(400, "security_exception", "token malformed"));
                    };
                    if t.get("refresh_used").and_then(Value::as_bool) == Some(true) || t.get("invalidated").and_then(Value::as_bool) == Some(true) {
                        return Err(EsError::new(400, "security_exception", "invalid_grant").with_meta("error_description", "token has already been refreshed"));
                    }
                    if let Value::Obj(m) = &mut t {
                        m.insert("refresh_used".into(), true.into());
                    }
                    let user = t.get("user").cloned().unwrap_or(Value::Null);
                    store::put(st, TOKENS, &k, t, now)?;
                    Ok(user)
                })
                .await?;
            let mut p = principal_from_user(&user);
            p.kind = AuthKind::Token;
            let auth = p.to_value();
            p.kind = AuthKind::Realm;
            issue(ctx, &p, true, auth).await
        }
        Some("_kerberos") => Err(EsError::security(401, "unable to authenticate user for action [cluster:admin/xpack/security/token/create]")),
        _ => Ok(Reply::Body(
            400,
            obj! {
                "error" => "unsupported_grant_type",
                "error_description" => "Validation Failed: 1: grant_type only supports the values: [password, _kerberos, refresh_token, client_credentials];",
            },
        )),
    }
}

pub async fn invalidate_token(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let token = body.get("token").map(Value::to_es_string);
    let refresh = body.get("refresh_token").map(Value::to_es_string);
    let username = body.get("username").map(Value::to_es_string);
    let realm = body.get("realm_name").map(Value::to_es_string);
    if token.is_none() && refresh.is_none() && username.is_none() && realm.is_none() {
        return Err(EsError::action_request_validation(&["one of [token, refresh_token, realm_name, username] must be specified".into()]));
    }
    // A token given by value is looked up in the tokens index: a missing index fails the
    // request, unless the value isn't even a token (`TokenService.invalidateAccessToken`,
    // `invalidateRefreshToken`).
    let by_value = token.as_ref().or(refresh.as_ref()).cloned();
    if let Some(v) = &by_value
        && !store::home_index(TOKENS).is_some_and(|i| ctx.state().metadata.indices.contains_key(i))
        && super::b64_decode(v).is_ok()
    {
        return Err(EsError::security(400, "failed to invalidate token"));
    }
    let now = ctx.now_ms;
    let (invalidated, previously) = ctx
        .cluster
        .update(move |st| {
            let mut targets: Vec<(String, Value)> = Vec::new();
            if let Some(t) = &token {
                if let Some(v) = st.metadata.xpack.get(TOKENS, &token_key(t)) {
                    targets.push((token_key(t), v.clone()));
                }
            } else if let Some(r) = &refresh {
                let rk = token_key(r);
                targets.extend(st.metadata.xpack.iter(TOKENS).filter(|(_, t)| t.get("refresh").and_then(Value::as_str) == Some(rk.as_str())).map(|(k, t)| (k.clone(), t.clone())));
            } else {
                for (k, t) in st.metadata.xpack.iter(TOKENS) {
                    let u = t.get("user");
                    if username.as_ref().is_some_and(|n| u.and_then(|u| u.get("username")).and_then(Value::as_str) != Some(n.as_str())) {
                        continue;
                    }
                    if realm.as_ref().is_some_and(|r| u.and_then(|u| u.get("realm")).and_then(Value::as_str) != Some(r.as_str())) {
                        continue;
                    }
                    targets.push((k.clone(), t.clone()));
                }
            }
            let (mut inv, mut prev) = (0usize, 0usize);
            let by_user = token.is_none() && refresh.is_none();
            for (k, mut t) in targets {
                // Invalidating by user counts access and refresh tokens separately.
                let n = if by_user && !t.get("refresh").is_none_or(Value::is_null) { 2 } else { 1 };
                if t.get("invalidated").and_then(Value::as_bool) == Some(true) {
                    prev += 1;
                    continue;
                }
                if let Value::Obj(m) = &mut t {
                    m.insert("invalidated".into(), true.into());
                }
                store::put(st, TOKENS, &k, t, now)?;
                inv += n;
            }
            Ok((inv, prev))
        })
        .await?;
    // No token of that value: 404, with the counts.
    let status = if by_value.is_some() && invalidated + previously == 0 { 404 } else { 200 };
    Ok(Reply::Body(status, obj! { "invalidated_tokens" => invalidated, "previously_invalidated_tokens" => previously, "error_count" => 0 }))
}

// --- service accounts ------------------------------------------------------------------------

fn service_accounts() -> &'static BTreeMap<String, Value> {
    static S: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    S.get_or_init(|| match crate::json::parse(include_bytes!("service_accounts.json")) {
        Ok(Some(Value::Obj(m))) => m.into_iter().collect(),
        _ => BTreeMap::new(),
    })
}

/// The role descriptor of a service account (`elastic/fleet-server`).
pub fn service_account_descriptor(account: &str) -> Option<Value> {
    service_accounts().get(account)?.get("role_descriptor").cloned()
}

fn check_account(ns: &str, svc: &str) -> EsResult<String> {
    let name = format!("{ns}/{svc}");
    if !service_accounts().contains_key(&name) {
        return Err(EsError::illegal_argument(format!("service account [{name}] does not exist")));
    }
    Ok(name)
}

fn authenticate_service_token(cluster: &Cluster, value: &str) -> EsResult<Principal> {
    use base64::Engine;
    let invalid = || super::unauthorized_credentials("oauth2 token: invalid token");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value.trim_end_matches('=')))
        .map_err(|_| invalid())?;
    if bytes.len() < 4 || bytes[..4] != [0, 1, 0, 1] {
        return Err(invalid());
    }
    let text = String::from_utf8_lossy(&bytes[4..]).into_owned();
    let Some((qualified, secret)) = text.split_once(':') else { return Err(invalid()) };
    let st = cluster.state();
    let Some(t) = st.metadata.xpack.get(SERVICE_TOKENS, qualified) else { return Err(invalid()) };
    if !super::verify_secret(secret, t.get("hash").and_then(Value::as_str).unwrap_or_default()) {
        return Err(invalid());
    }
    let (account, name) = qualified.rsplit_once('/').unwrap_or((qualified, ""));
    Ok(Principal {
        username: account.to_string(),
        roles: Vec::new(),
        full_name: Value::from(format!("Service account - {account}")),
        email: Value::Null,
        metadata: obj! { "_elastic_service_account" => true },
        realm_name: "_service_account",
        realm_type: "_service_account",
        authenticating: None,
        kind: AuthKind::ServiceAccount { token: name.to_string() },
    })
}

pub fn get_service_accounts(ctx: &Ctx) -> EsResult<Reply> {
    let ns = ctx.path_param("namespace");
    let svc = ctx.path_param("service");
    let mut out = Map::new();
    for (k, v) in service_accounts() {
        let (n, s) = k.split_once('/').unwrap_or((k, ""));
        if ns.is_some_and(|x| x != n) || svc.is_some_and(|x| x != s) {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    Reply::ok(Value::Obj(out))
}

pub async fn create_service_token(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let ns = ctx.path_param("namespace").unwrap_or_default().to_string();
    let svc = ctx.path_param("service").unwrap_or_default().to_string();
    let account = check_account(&ns, &svc)?;
    let name = ctx.path_param("name").map(str::to_string).unwrap_or_else(|| format!("token_{}", super::random_b64url(15)));
    let valid = !name.is_empty() && name.len() <= 256 && !name.starts_with('_') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !valid {
        return Err(EsError::action_request_validation(&[
            "service account token name must have at least 1 character and at most 256 characters, contain alphanumeric characters (a-z, A-Z, 0-9), dashes (-) and underscores (_), and must not begin with an underscore (_)".into(),
        ]));
    }
    let qualified = format!("{account}/{name}");
    let secret = super::random_b64url(16);
    let doc = obj! { "hash" => super::hash_secret(&secret), "created" => ctx.now_ms };
    let now = ctx.now_ms;
    let q = qualified.clone();
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(SERVICE_TOKENS, &q).is_some() {
                let uuid = st.metadata.indices.get(".security-7").map(|i| i.uuid.clone()).unwrap_or_else(|| "_na_".into());
                return Err(EsError::new(409, "version_conflict_engine_exception", format!("[service_account_token-{q}]: version conflict, document already exists (current version [1])"))
                    .with_meta("index_uuid", uuid)
                    .with_meta("shard", "0")
                    .with_meta("index", ".security-7"));
            }
            store::put(st, SERVICE_TOKENS, &q, doc, now).map(|_| ())
        })
        .await?;
    use base64::Engine;
    let mut raw = vec![0u8, 1, 0, 1];
    raw.extend_from_slice(format!("{qualified}:{secret}").as_bytes());
    let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    Reply::ok(obj! { "created" => true, "token" => obj! { "name" => name, "value" => value } })
}

pub async fn delete_service_token(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let ns = ctx.path_param("namespace").unwrap_or_default().to_string();
    let svc = ctx.path_param("service").unwrap_or_default().to_string();
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let q = format!("{ns}/{svc}/{name}");
    let found = ctx.cluster.update(move |st| Ok(store::remove(st, SERVICE_TOKENS, &q).is_some())).await?;
    Ok(Reply::Body(if found { 200 } else { 404 }, obj! { "found" => found }))
}

pub fn get_service_credentials(ctx: &Ctx) -> EsResult<Reply> {
    let ns = ctx.path_param("namespace").unwrap_or_default();
    let svc = ctx.path_param("service").unwrap_or_default();
    let account = check_account(ns, svc)?;
    let st = ctx.state();
    let prefix = format!("{account}/");
    let tokens: Map = st.metadata.xpack.iter(SERVICE_TOKENS).filter_map(|(k, _)| k.strip_prefix(&prefix).map(|n| (n.to_string(), obj! {}))).collect();
    Reply::ok(obj! {
        "service_account" => account,
        "count" => tokens.len(),
        "tokens" => Value::Obj(tokens),
        "nodes_credentials" => obj! { "_nodes" => obj! { "total" => 1, "successful" => 1, "failed" => 0 }, "file_tokens" => obj! {} },
    })
}

// --- user profiles ---------------------------------------------------------------------------

/// A user's profile uid (`ProfileService`): `u_`, the base64url SHA-256 of the username, and
/// the first differentiator (`_0`…`_9`) that is free or already the user's of that realm.
fn profile_uid(st: &crate::meta::ClusterState, username: &str, realm: &str) -> EsResult<String> {
    use base64::Engine;
    use sha2::Digest;
    let d = sha2::Sha256::digest(username.as_bytes());
    let base = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d);
    for n in 0..10 {
        let uid = format!("u_{base}_{n}");
        match st.metadata.xpack.get(PROFILES, &uid) {
            None => return Ok(uid),
            Some(p) if p.pointer(&["user", "username"]).and_then(Value::as_str) == Some(username) && p.pointer(&["user", "realm_name"]).and_then(Value::as_str) == Some(realm) => {
                return Ok(uid);
            }
            Some(_) => {}
        }
    }
    Err(EsError::illegal_state(format!("failed to activate profile for user [{username}]: too many profiles with the same username")))
}

/// The profile uid of a user, if it activated one.
pub fn profile_uid_of(st: &crate::meta::ClusterState, username: &str) -> Option<String> {
    st.metadata
        .xpack
        .iter(PROFILES)
        .find(|(_, p)| p.pointer(&["user", "username"]).and_then(Value::as_str) == Some(username))
        .map(|(k, _)| k.clone())
}

fn profile_view(p: &Value, data_keys: Option<&[String]>) -> Value {
    let mut out = Map::new();
    for k in ["uid", "enabled", "last_synchronized", "user", "labels"] {
        out.insert(k.into(), p.get(k).cloned().unwrap_or(Value::Null));
    }
    let data = match (data_keys, p.get("data")) {
        (Some(keys), Some(Value::Obj(d))) => Value::Obj(
            d.iter()
                .filter(|(k, _)| keys.iter().any(|w| w == "*" || crate::xcontent::filter::glob_match(w, k)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => Value::object(),
    };
    out.insert("data".into(), data);
    out.insert("_doc".into(), obj! { "_primary_term" => 1, "_seq_no" => p.get("seq_no").cloned().unwrap_or(Value::from(0)) });
    Value::Obj(out)
}

pub async fn activate_profile(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let p = grant_principal(ctx.cluster, &body, "cluster:admin/xpack/security/profile/activate")?;
    let now = ctx.now_ms;
    let mut user = Map::new();
    user.insert("username".into(), p.username.clone().into());
    user.insert("roles".into(), Value::Arr(p.roles.iter().map(Value::from).collect()));
    user.insert("realm_name".into(), p.realm_name.into());
    if !p.full_name.is_null() {
        user.insert("full_name".into(), p.full_name.clone());
    }
    if !p.email.is_null() {
        user.insert("email".into(), p.email.clone());
    }
    let doc = ctx
        .cluster
        .update(move |st| {
            let u = profile_uid(st, &p.username, p.realm_name)?;
            let existing = st.metadata.xpack.get(PROFILES, &u).cloned();
            let seq = existing.as_ref().and_then(|e| e.get("seq_no")).and_then(Value::as_i64).map_or(0, |s| s + 1);
            let doc = obj! {
                "uid" => u.clone(),
                "enabled" => existing.as_ref().and_then(|e| e.get("enabled")).and_then(Value::as_bool).unwrap_or(true),
                "last_synchronized" => now,
                "user" => Value::Obj(user),
                "labels" => existing.as_ref().and_then(|e| e.get("labels")).cloned().unwrap_or_else(Value::object),
                "data" => existing.as_ref().and_then(|e| e.get("data")).cloned().unwrap_or_else(Value::object),
                "seq_no" => seq,
            };
            store::put(st, PROFILES, &u, doc.clone(), now)?;
            Ok(doc)
        })
        .await?;
    Reply::ok(profile_view(&doc, None))
}

fn data_param(ctx: &Ctx) -> Option<Vec<String>> {
    ctx.list_param("data")
}

pub fn get_profile(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let uids = ctx.list_param("uid").unwrap_or_default();
    let data = data_param(ctx);
    let mut profiles = Vec::new();
    let mut errors = Map::new();
    for u in &uids {
        match st.metadata.xpack.get(PROFILES, u) {
            Some(p) => profiles.push(profile_view(p, data.as_deref())),
            None => {
                errors.insert(u.clone(), obj! { "type" => "resource_not_found_exception", "reason" => "profile document not found" });
            }
        }
    }
    let mut out = Map::new();
    out.insert("profiles".into(), Value::Arr(profiles));
    if !errors.is_empty() {
        out.insert("errors".into(), obj! { "count" => errors.len(), "details" => Value::Obj(errors) });
    }
    Reply::ok(Value::Obj(out))
}

async fn update_profile(ctx: &Ctx<'_>, f: impl FnOnce(&mut Map) + Send + 'static) -> EsResult<Reply> {
    let uid = ctx.path_param("uid").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(mut p) = st.metadata.xpack.get(PROFILES, &uid).cloned() else {
                return Err(EsError::resource_not_found("profile document not found"));
            };
            if let Value::Obj(m) = &mut p {
                f(m);
                let seq = m.get("seq_no").and_then(Value::as_i64).unwrap_or(0) + 1;
                m.insert("seq_no".into(), seq.into());
            }
            store::put(st, PROFILES, &uid, p, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn set_profile_enabled(ctx: &Ctx<'_>, enabled: bool) -> EsResult<Reply> {
    update_profile(ctx, move |m| {
        m.insert("enabled".into(), enabled.into());
    })
    .await
}

fn merge(into: &mut Map, from: &Map) {
    for (k, v) in from {
        match (into.get_mut(k), v) {
            (Some(Value::Obj(a)), Value::Obj(b)) => merge(a, b),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

pub async fn update_profile_data(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    if body.get("labels").is_none() && body.get("data").is_none() {
        return Err(EsError::action_request_validation(&["one of labels or data is required".into()]));
    }
    update_profile(ctx, move |m| {
        for key in ["labels", "data"] {
            if let Some(Value::Obj(new)) = body.get(key) {
                let mut cur = m.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
                merge(&mut cur, new);
                m.insert(key.into(), Value::Obj(cur));
            }
        }
    })
    .await
}

pub fn suggest_profiles(ctx: &Ctx) -> EsResult<Reply> {
    let started = std::time::Instant::now();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let name = body.get("name").map(Value::to_es_string).or_else(|| ctx.query("name").map(str::to_string)).unwrap_or_default().to_lowercase();
    let size = body.get("size").and_then(Value::as_i64).unwrap_or(10).max(0) as usize;
    let data = body.get("data").map(|d| match d {
        Value::Arr(a) => a.iter().map(Value::to_es_string).collect::<Vec<_>>(),
        other => other.to_es_string().split(',').map(str::to_string).collect(),
    });
    let st = ctx.state();
    let mut hits = Vec::new();
    for (_, p) in st.metadata.xpack.iter(PROFILES) {
        if p.get("enabled").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let user = p.get("user").cloned().unwrap_or(Value::Null);
        let matched = name.is_empty()
            || ["username", "full_name", "email"].iter().any(|f| {
                user.get(f).and_then(Value::as_str).is_some_and(|s| {
                    let s = s.to_lowercase();
                    s.starts_with(&name) || s.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&name))
                })
            });
        if matched {
            let mut v = Map::new();
            v.insert("uid".into(), p.get("uid").cloned().unwrap_or(Value::Null));
            v.insert("user".into(), user);
            v.insert("labels".into(), p.get("labels").cloned().unwrap_or_else(Value::object));
            let full = profile_view(p, data.as_deref());
            v.insert("data".into(), full.get("data").cloned().unwrap_or_else(Value::object));
            hits.push(Value::Obj(v));
        }
    }
    let total = hits.len();
    hits.truncate(size);
    Reply::ok(obj! {
        "took" => started.elapsed().as_millis() as i64,
        "total" => obj! { "value" => total, "relation" => "eq" },
        "profiles" => Value::Arr(hits),
    })
}

pub fn has_privileges_profile(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let st = ctx.state();
    let file = super::file_roles(ctx.cluster);
    let uids: Vec<String> = body.get("uids").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    let privs = body.get("privileges").cloned().unwrap_or_else(Value::object);
    let mut has = Vec::new();
    let mut errors = Map::new();
    for u in uids {
        let Some(p) = st.metadata.xpack.get(PROFILES, &u) else {
            errors.insert(u, obj! { "type" => "resource_not_found_exception", "reason" => "profile document not found" });
            continue;
        };
        let user = p.get("user").cloned().unwrap_or(Value::Null);
        let principal = Principal {
            username: user.get("username").map(Value::to_es_string).unwrap_or_default(),
            roles: user.get("roles").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default(),
            full_name: Value::Null,
            email: Value::Null,
            metadata: Value::object(),
            realm_name: "default_native",
            realm_type: "native",
            authenticating: None,
            kind: AuthKind::Realm,
        };
        let (all, ..) = super::users::check_privileges(&file, &st, &principal, &privs)?;
        if all {
            has.push(Value::from(u));
        }
    }
    let mut out = Map::new();
    out.insert("has_privilege_uids".into(), Value::Arr(has));
    if !errors.is_empty() {
        out.insert("errors".into(), obj! { "count" => errors.len(), "details" => Value::Obj(errors) });
    }
    Reply::ok(Value::Obj(out))
}

// --- misc ------------------------------------------------------------------------------------

pub fn authenticate_api(ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(super::principal(ctx).to_value())
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let mut nodes = Map::new();
    nodes.insert(
        ctx.cluster.node.id.clone(),
        obj! { "roles" => obj! { "dls" => obj! { "bit_set_cache" => obj! {
            "count" => 0, "memory" => "0b", "memory_in_bytes" => 0, "hits" => 0, "misses" => 0, "evictions" => 0,
            "hits_time_in_millis" => 0, "misses_time_in_millis" => 0,
        } } } },
    );
    Reply::ok(obj! { "nodes" => Value::Obj(nodes) })
}

const SETTINGS: &str = "security.settings";

pub fn get_settings(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let mut out = Map::new();
    for (key, index) in [("security", ".security-7"), ("security-tokens", ".security-tokens-7"), ("security-profile", ".security-profile-8")] {
        let mut settings = Map::new();
        // An index's settings once it exists (the main index is listed always).
        if index == ".security-7" || st.metadata.indices.contains_key(index) {
            settings.insert("auto_expand_replicas".into(), "0-1".into());
            settings.insert("number_of_replicas".into(), "0".into());
            settings.insert("routing".into(), obj! { "allocation" => obj! { "include" => obj! { "_tier_preference" => "data_hot,data_content" } } });
        }
        if let Some(Value::Obj(over)) = st.metadata.xpack.get(SETTINGS, key) {
            for (k, v) in over {
                let k = k.strip_prefix("index.").unwrap_or(k).to_string();
                if v.is_null() {
                    settings.shift_remove(&k);
                } else {
                    settings.insert(k, v.clone());
                }
            }
        }
        out.insert(key.into(), if settings.is_empty() { obj! {} } else { obj! { "index" => Value::Obj(settings) } });
    }
    Reply::ok(Value::Obj(out))
}

pub async fn update_settings(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            for key in ["security", "security-tokens", "security-profile"] {
                if let Some(Value::Obj(m)) = body.get(key) {
                    let mut cur = st.metadata.xpack.get(SETTINGS, key).and_then(Value::as_object).cloned().unwrap_or_default();
                    for (k, v) in m {
                        let v = match v {
                            Value::Num(_) | Value::Bool(_) => Value::from(v.to_es_string()),
                            other => other.clone(),
                        };
                        cur.insert(k.clone(), v);
                    }
                    store::put(st, SETTINGS, key, Value::Obj(cur), now)?;
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_look_like_elasticsearch_tokens() {
        let t = new_token();
        assert_eq!(t.len(), 40);
        assert_ne!(new_token(), t);
        assert_eq!(token_key("a"), token_key("a"));
    }

    #[test]
    fn service_accounts_load() {
        assert!(service_accounts().contains_key("elastic/fleet-server"));
        assert!(service_account_descriptor("elastic/kibana").is_some());
        assert!(check_account("elastic", "nope").is_err());
    }

}
