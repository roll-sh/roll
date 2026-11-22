//! OAuth 2.1 for MCP clients (docs/mcp.md, "Authorization"): the MCP authorization spec
//! (2025-06-18 and 2025-11-25). Protected resource metadata (RFC 9728), authorization server
//! metadata (RFC 8414), dynamic client registration (RFC 7591), authorization codes with PKCE
//! (S256 only), resource indicators (RFC 8707), rotating refresh tokens and revocation (RFC 7009).
//!
//! The authorization endpoint is the dashboard's consent screen (`/oauth/authorize`): the signed-in
//! operator sees what the client asks for, narrows it, and the dashboard posts the grant to
//! [`authorize`], which answers the redirect with a one-minute, one-use code.

use crate::store::{ACCESS_TTL_MS, NewGrant};
use crate::{Access, Error};
use axum::extract::State;
use axum::http::{Extensions, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

type S = State<Arc<Access>>;

/// The public endpoints: discovery, registration, token, revocation. They go in front of the gate
/// (`Access::Public`); `/v1/access/oauth/authorize` is the operator's and sits behind it.
pub fn public_router(access: Arc<Access>) -> Router {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource_metadata))
        .route("/.well-known/oauth-protected-resource/v1/mcp", get(resource_metadata))
        .route("/.well-known/oauth-authorization-server", get(server_metadata))
        .route("/.well-known/oauth-authorization-server/v1/mcp", get(server_metadata))
        .route("/.well-known/openid-configuration", get(server_metadata))
        .route("/v1/access/oauth/register", post(register))
        .route("/v1/access/oauth/token", post(token))
        .route("/v1/access/oauth/revoke", post(revoke))
        .route("/v1/access/oauth/device_authorization", post(device_authorization))
        // GitHub's secret scanning reports: their signature is the credential.
        .route("/v1/access/leaks/github", post(crate::leaks::github))
        .with_state(access)
}

/// The scopes a client may ask for: each is a preset the consent screen starts from.
pub const SCOPES: &[&str] = &["read-only", "agent", "project", "development", "previews", "full"];

fn api(a: &Access) -> String {
    a.config.api_url.trim_end_matches('/').to_string()
}

/// The MCP server's resource identifier.
pub fn resource(a: &Access) -> String {
    format!("{}/v1/mcp", api(a))
}

async fn resource_metadata(State(a): S) -> Json<Value> {
    Json(json!({
        "resource": resource(&a),
        "authorization_servers": [api(&a)],
        "scopes_supported": SCOPES,
        "bearer_methods_supported": ["header"],
        "resource_name": "roll",
        "resource_documentation": "https://roll.sh/docs/mcp",
    }))
}

async fn server_metadata(State(a): S) -> Json<Value> {
    let api = api(&a);
    Json(json!({
        "issuer": api,
        "authorization_endpoint": format!("{}/oauth/authorize", a.config.web_url.trim_end_matches('/')),
        "token_endpoint": format!("{api}/v1/access/oauth/token"),
        "registration_endpoint": format!("{api}/v1/access/oauth/register"),
        "revocation_endpoint": format!("{api}/v1/access/oauth/revoke"),
        "device_authorization_endpoint": format!("{api}/v1/access/oauth/device_authorization"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token", DEVICE_GRANT, crate::client_credentials::GRANT, crate::client_credentials::JWT_GRANT],
        "code_challenge_methods_supported": ["S256"],
        // Public clients (none); machine clients by secret or by a key they sign with.
        "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post", "private_key_jwt"],
        "token_endpoint_auth_signing_alg_values_supported": crate::client_credentials::ALGS,
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": SCOPES,
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
        // Identity Assertion JWT Authorization Grants (ID-JAG, draft-ietf-oauth-identity-assertion-
        // authz-grant §7.2): what roll implements, never which providers it trusts.
        "authorization_grant_profiles_supported": [crate::enterprise::ID_JAG_PROFILE],
        "id_jag_signing_alg_values_supported": ["RS256", "ES256"],
    }))
}

/// A redirect URI a client may register: https, http on loopback only (RFC 8252), or an app's
/// private-use scheme (`cursor://…`); never a scheme a browser runs (`javascript:`, `data:`).
pub fn redirect_ok(uri: &str) -> bool {
    let Ok(u) = url::Url::parse(uri) else { return false };
    if u.fragment().is_some() || uri.len() > 2000 {
        return false;
    }
    match u.scheme() {
        "https" => u.host_str().is_some(),
        "http" => matches!(u.host(), Some(url::Host::Domain("localhost"))) || matches!(u.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback()) || matches!(u.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback()),
        "javascript" | "data" | "file" | "blob" | "about" | "vbscript" | "ftp" | "ws" | "wss" => false,
        s => s.len() >= 3 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
    }
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    client_uri: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
}

fn too_many(desc: &str) -> Response {
    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], Json(json!({ "error": "too_many_requests", "error_description": desc }))).into_response()
}

fn reg_error(code: &'static str, desc: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": code, "error_description": desc }))).into_response()
}

/// Registrations from one network (/24, /48) a minute at most, on a node: registration is open.
const REGISTER_PER_MINUTE: u32 = 20;

/// RFC 7591: open registration of public clients (PKCE, no secret).
async fn register(State(a): S, req: axum::extract::Request) -> Response {
    let ip = crate::evaluate::client_ip(&req, &a.config);
    let network = ip.and_then(|i| crate::store::network_of(&i.to_string())).unwrap_or_else(|| "unknown".into());
    let admitted = a.registrations.lock().unwrap_or_else(|e| e.into_inner()).admit(&network, code_sql::now_ms(), REGISTER_PER_MINUTE as usize);
    match admitted {
        Ok(()) => {}
        Err(crate::Refused::Busy) => return too_many("too many registrations lately: try again in a minute"),
        Err(crate::Refused::Key) => return too_many("too many registrations from this network: try again in a minute"),
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), (16 << 10) + 1).await else { return reg_error("invalid_client_metadata", "the registration is too large") };
    if body.len() > 16 << 10 {
        return reg_error("invalid_client_metadata", "the registration is too large");
    }
    let Ok(r) = serde_json::from_slice::<Registration>(&body) else { return reg_error("invalid_client_metadata", "the body must be JSON client metadata") };
    // A client that only signs devices in (RFC 8628) has nowhere to be sent back to.
    let device_only = r.grant_types.as_ref().is_some_and(|g| g.iter().any(|t| t == DEVICE_GRANT) && !g.iter().any(|t| t == "authorization_code"));
    if (r.redirect_uris.is_empty() && !device_only) || r.redirect_uris.len() > 10 || !r.redirect_uris.iter().all(|u| redirect_ok(u)) {
        return reg_error("invalid_redirect_uri", "redirect_uris: 1-10 URIs, each https, http on localhost, or an app scheme");
    }
    if r.token_endpoint_auth_method.as_deref().is_some_and(|m| m != "none") {
        return reg_error("invalid_client_metadata", "only public clients (token_endpoint_auth_method \"none\", with PKCE) can register");
    }
    if r.grant_types.as_ref().is_some_and(|g| g.iter().any(|t| t != "authorization_code" && t != "refresh_token" && t != DEVICE_GRANT)) {
        return reg_error("invalid_client_metadata", "grant_types: authorization_code, refresh_token and urn:ietf:params:oauth:grant-type:device_code");
    }
    // A name as consent and agents show it: no control or invisible characters, no backticks, at
    // most 64 (what a token's name may be).
    let cleaned: String = r.client_name.as_deref().unwrap_or("").chars().filter(|c| shown(*c)).take(64).collect();
    let name = if cleaned.trim().is_empty() { "MCP client".to_string() } else { cleaned.trim().to_string() };
    let uri = r.client_uri.filter(|u| u.starts_with("https://") && u.len() <= 500);
    let uris = r.redirect_uris.clone();
    let client = match tokio::task::spawn_blocking(move || a.store.register_client(&name, uri.as_deref(), &uris)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return e.into_response(),
        Err(e) => return Error::Internal(e.to_string()).into_response(),
    };
    let mut out = json!({
        "client_id": client.id,
        "client_name": client.name,
        "redirect_uris": client.redirect_uris,
        "grant_types": if device_only { json!([DEVICE_GRANT, "refresh_token"]) } else { json!(["authorization_code", "refresh_token"]) },
        "response_types": if device_only { json!([]) } else { json!(["code"]) },
        "token_endpoint_auth_method": "none",
        "client_id_issued_at": client.created_at / 1000,
    });
    // RFC 7591: a field the client didn't set is left out, never null (clients' schemas refuse it).
    if let Some(uri) = client.uri {
        out["client_uri"] = Value::String(uri);
    }
    (StatusCode::CREATED, Json(out)).into_response()
}

#[derive(Deserialize)]
pub struct Consent {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    /// Step-up: the id (`stp_…`) of the step-up roll recorded when this client's connection was
    /// refused (its `insufficient_scope` challenge). Its one rule joins that connection; the
    /// consent's other fields aren't used.
    #[serde(default)]
    pub step_up: Option<String>,
    #[serde(flatten)]
    pub allowed: Allowed,
}

/// What a person allows a client, on the consent screen or for a device.
#[derive(Deserialize)]
pub struct Allowed {
    /// The connection's name (default: the client's).
    #[serde(default)]
    pub name: Option<String>,
    pub rules: roll_auth::scope::Scope,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub ip_allow: Vec<String>,
    /// Requests a minute the connection may make.
    #[serde(default)]
    pub rate_per_minute: Option<u32>,
    /// What the rules don't allow asks the person instead of being refused.
    #[serde(default)]
    pub ask_otherwise: bool,
}

/// A consent's grant, before it's written: whose it is, and whether it waits for an admin.
struct Consented {
    new: NewGrant,
    by: String,
    user: Option<String>,
    /// The person's handle when it waits for an admin (for the admins' mail).
    asker: Option<String>,
}

/// Who consents and what their grant is: the operator's or a person's (whose connection acts as
/// them, and waits for an admin past what the policy has one approve, as their tokens do); never
/// a token's.
fn consented(a: &Arc<Access>, ext: &Extensions, client: &crate::store::Client, allowed: &Allowed) -> crate::Result<Consented> {
    let mut asker = None;
    let user = match crate::api::who(a, ext)? {
        // A signed-in admin's connection is theirs (it acts as the operator while they're one).
        crate::api::Who::Operator => ext.get::<roll_auth::SignedIn>().map(|u| u.user.clone()),
        crate::api::Who::User(u) => {
            asker = Some(u.handle);
            Some(u.user)
        }
        crate::api::Who::Grant(_) => return Err(Error::Forbidden("a token never connects an MCP client".into())),
    };
    a.check_policy(allowed.expires_at)?;
    let new = NewGrant {
        name: allowed.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| client.name.clone()),
        rules: allowed.rules.clone(),
        expires_at: allowed.expires_at,
        ip_allow: allowed.ip_allow.clone(),
        rate_per_minute: allowed.rate_per_minute,
        ask_otherwise: allowed.ask_otherwise,
        ..Default::default()
    };
    let asker = asker.filter(|_| a.policy_now().needs_approval(&new.rules, new.ask_otherwise));
    Ok(Consented { new: NewGrant { pending: asker.is_some(), ..new }, by: crate::api::actor(ext), user, asker })
}

/// Mail a consent's news: its person hears what was added to their account, and admins hear of
/// one that waits for them.
async fn tell_consented(a: &Arc<Access>, ext: &Extensions, asker: Option<String>, grant_id: String, added: crate::api::Added<'_>) {
    let a2 = a.clone();
    let Ok(Ok(g)) = tokio::task::spawn_blocking(move || a2.store.get(&grant_id)).await else { return };
    a.tell_added(&g, added, ext);
    if let Some(handle) = asker {
        a.tell_admins_waiting(&g, &handle);
    }
}

/// The consent screen's answer, from the operator or a signed-in user (whose connection is
/// their own: it acts as them): a code for the client, in the redirect the dashboard sends the
/// browser to.
pub async fn authorize(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Response> {
    let c: Consent = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    if c.code_challenge_method.as_deref().unwrap_or("S256") != "S256" {
        return Err(Error::BadRequest("code_challenge_method must be S256".into()));
    }
    if !(43..=128).contains(&c.code_challenge.len()) || !c.code_challenge.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(Error::BadRequest("code_challenge must be a base64url SHA-256".into()));
    }
    if let Some(r) = &c.resource
        && r.trim_end_matches('/') != resource(&a)
    {
        return Err(Error::BadRequest(format!("resource must be {}", resource(&a))));
    }
    let client = known_client(&a, &c.client_id).await?.ok_or_else(|| Error::BadRequest("unknown client_id".into()))?;
    if !client.redirect_uris.contains(&c.redirect_uri) {
        return Err(Error::BadRequest("redirect_uri isn't one the client registered".into()));
    }
    if !a.policy_now().allows_client(&client) {
        return Err(Error::Forbidden(crate::evaluate::CLIENT_NOT_ALLOWED.into()));
    }
    // The resource the client asked for (RFC 8707; MCP clients always name the MCP server): its
    // tokens are for that server only. One that named none (`roll login`) gets API tokens.
    let res = c.resource.as_ref().map(|_| resource(&a));
    if let Some(id) = c.step_up.clone() {
        return step_up(&a, &ext, &c, &client, id, res).await;
    }
    let Consented { new, by, user, asker } = consented(&a, &ext, &client, &c.allowed)?;
    let (a2, cl, uri, ch) = (a.clone(), client.clone(), c.redirect_uri.clone(), c.code_challenge.clone());
    let (code, grant_id) = tokio::task::spawn_blocking(move || a2.store.issue_code(&cl, &new, crate::store::Issuer { created_by: &by, user: user.as_deref() }, &uri, &ch, res.as_deref()))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    tell_consented(&a, &ext, asker, grant_id.clone(), crate::api::Added::Client(&client.name)).await;
    let mut redirect = url::Url::parse(&c.redirect_uri).map_err(|_| Error::BadRequest("redirect_uri isn't a URL".into()))?;
    {
        let mut q = redirect.query_pairs_mut();
        q.append_pair("code", &code);
        if let Some(s) = &c.state {
            q.append_pair("state", s);
        }
        q.append_pair("iss", &api(&a));
    }
    let mut res = Json(json!({ "redirect": redirect.to_string() })).into_response();
    res.extensions_mut().insert(crate::api::Changed(grant_id));
    Ok(res)
}

/// The step-up `id` and its connection, when it's the caller's own live connection of `client`
/// (any client when `None`) and not waiting on an admin: else one answer for every other case, so
/// an id tells nothing about whose it is.
pub(crate) async fn own_step_up(
    a: &Arc<Access>,
    ext: &Extensions,
    id: &str,
    client: Option<&str>,
) -> crate::Result<(crate::store::Grant, roll_auth::scope::Rule, Option<String>)> {
    let (user, asker) = match crate::api::who(a, ext)? {
        crate::api::Who::Operator => (ext.get::<roll_auth::SignedIn>().map(|u| u.user.clone()), None),
        crate::api::Who::User(u) => (Some(u.user), Some(u.handle)),
        crate::api::Who::Grant(_) => return Err(Error::Forbidden("a token never connects an MCP client".into())),
    };
    let none = || Error::BadRequest("no such step-up of yours: retry the call that was refused".into());
    let (a2, id2) = (a.clone(), id.to_string());
    let (gid, rule) = tokio::task::spawn_blocking(move || a2.store.step_up(&id2)).await.map_err(|e| Error::Internal(e.to_string()))??.ok_or_else(none)?;
    let a2 = a.clone();
    let g = tokio::task::spawn_blocking(move || a2.store.get(&gid)).await.map_err(|e| Error::Internal(e.to_string()))?.map_err(|_| none())?;
    let theirs = g.kind == "oauth" && g.user == user && g.live(code_sql::now_ms()) && client.is_none_or(|c| g.client.as_ref().is_some_and(|cl| cl.id == c));
    if !theirs {
        return Err(none());
    }
    if g.pending {
        return Err(Error::Conflict("that connection waits for an admin's approval: its rules can't change until they decide".into()));
    }
    Ok((g, rule, asker))
}

/// Step-up's consent: the step-up's one rule joins the person's connection (read and written in
/// one transaction; nothing else about it changes), the policy and an admin's approval applying as
/// to any widened token, and the client gets a code for that connection: its other sessions widen
/// too and nothing new is made.
async fn step_up(a: &Arc<Access>, ext: &Extensions, c: &Consent, client: &crate::store::Client, id: String, res: Option<String>) -> crate::Result<Response> {
    let (g, _rule, asker) = own_step_up(a, ext, &id, Some(&client.id)).await?;
    // The token policy as it is now, as editing the connection would meet it.
    a.check_policy(g.expires_at)?;
    let policy = a.policy_now();
    let person = asker.is_some();
    let (a2, id2, gid) = (a.clone(), id.clone(), g.id.clone());
    let (updated, added) = tokio::task::spawn_blocking(move || a2.store.apply_step_up(&id2, &gid, &|rules, ask| person && policy.needs_approval(rules, ask)))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    if added {
        a.tell_added(&updated, crate::api::Added::Client(&client.name), ext);
        if updated.pending && let Some(h) = &asker {
            a.tell_admins_waiting(&updated, h);
        }
    }
    let (a2, cid, gid, uri, ch) = (a.clone(), client.id.clone(), g.id.clone(), c.redirect_uri.clone(), c.code_challenge.clone());
    let code = tokio::task::spawn_blocking(move || a2.store.issue_code_for(&cid, &gid, &uri, &ch, res.as_deref())).await.map_err(|e| Error::Internal(e.to_string()))??;
    let mut redirect = url::Url::parse(&c.redirect_uri).map_err(|_| Error::BadRequest("redirect_uri isn't a URL".into()))?;
    {
        let mut q = redirect.query_pairs_mut();
        q.append_pair("code", &code);
        if let Some(s) = &c.state {
            q.append_pair("state", s);
        }
        q.append_pair("iss", &api(a));
    }
    let mut out = Json(json!({ "redirect": redirect.to_string() })).into_response();
    out.extensions_mut().insert(crate::api::Changed(g.id));
    Ok(out)
}

/// `GET /v1/access/oauth/step-ups/{id}`: what a step-up proposes, for its person's consent screen
/// (the connection and the one rule), or one "no such step-up" for anyone else.
pub async fn step_up_view(State(a): S, ext: Extensions, axum::extract::Path(id): axum::extract::Path<String>) -> crate::Result<Json<Value>> {
    let (g, rule, asker) = own_step_up(&a, &ext, &id, None).await?;
    a.check_policy(g.expires_at)?;
    let mut rules = g.rules.clone();
    rules.0.push(rule.clone());
    let waits = asker.is_some() && a.policy_now().needs_approval(&rules, g.ask_otherwise);
    Ok(Json(json!({ "id": id, "connection": { "id": g.id, "name": g.name, "client_id": g.client.as_ref().map(|c| c.id.clone()) }, "rule": rule, "waits_for_admin": waits })))
}

/// A metadata document is fetched at most this often (it's kept between).
const METADATA_TTL_MS: i64 = 3600 * 1000;
/// A metadata document's size, at most.
const METADATA_MAX: usize = 64 << 10;

/// A client by its id: registered here, or (an https URL, MCP 2025-11-25) known by the metadata
/// document at that URL, fetched when it isn't known or is an hour old.
pub async fn known_client(a: &Arc<Access>, id: &str) -> crate::Result<Option<crate::store::Client>> {
    let (a2, id2) = (a.clone(), id.to_string());
    let stored = tokio::task::spawn_blocking(move || a2.store.client(&id2)).await.map_err(|e| Error::Internal(e.to_string()))??;
    let document = id.starts_with("https://") || (a.config.metadata_allow_private && id.starts_with("http://"));
    if !document {
        return Ok(stored);
    }
    if let Some(c) = &stored
        && code_sql::now_ms() - c.created_at < METADATA_TTL_MS
    {
        return Ok(stored);
    }
    let fetched = fetch_metadata(id, a.config.metadata_allow_private).await.map_err(Error::BadRequest)?;
    let (a2, f2) = (a.clone(), fetched.clone());
    tokio::task::spawn_blocking(move || a2.store.put_metadata_client(&f2)).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(Some(fetched))
}

/// A client ID metadata document: `id` is its URL (https, with a path, no fragment). Fetched
/// through roll's private-address guard (`roll-net`), without redirects, in 5 seconds, at most
/// 64 KiB; it must name itself (`client_id` is the URL) and list redirects a client may use.
pub async fn fetch_metadata(id: &str, allow_private: bool) -> Result<crate::store::Client, String> {
    let url = url::Url::parse(id).map_err(|_| "client_id isn't a URL".to_string())?;
    if url.scheme() != "https" && !(allow_private && url.scheme() == "http") {
        return Err("a client metadata document is https".into());
    }
    if url.fragment().is_some() || url.path() == "/" || url.path().is_empty() || id.len() > 2000 {
        return Err("a client metadata URL has a path and no fragment".into());
    }
    if !allow_private && roll_net::literal_private_host(&url) {
        return Err("a client metadata document can't be at a private address".into());
    }
    let mut b = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(5)).no_proxy();
    if !allow_private {
        b = b.dns_resolver(roll_net::PublicOnly);
    }
    let client = b.build().map_err(|e| e.to_string())?;
    let mut resp = client.get(url.clone()).header("accept", "application/json").send().await.map_err(|e| format!("fetching the client's metadata: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("the client's metadata answered {}", resp.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > METADATA_MAX {
            return Err("the client's metadata is over 64 KiB".into());
        }
    }
    let doc: Value = serde_json::from_slice(&body).map_err(|_| "the client's metadata isn't JSON".to_string())?;
    if doc["client_id"].as_str() != Some(id) {
        return Err("the client's metadata names another client_id".into());
    }
    if doc["token_endpoint_auth_method"].as_str().is_some_and(|m| m != "none") {
        return Err("only public clients (token_endpoint_auth_method \"none\", with PKCE) can connect".into());
    }
    let redirect_uris: Vec<String> = doc["redirect_uris"].as_array().into_iter().flatten().filter_map(|u| u.as_str().map(str::to_string)).collect();
    if redirect_uris.is_empty() || redirect_uris.len() > 10 || !redirect_uris.iter().all(|u| redirect_ok(u)) {
        return Err("the client's metadata needs 1-10 redirect_uris, each https, http on localhost, or an app scheme".into());
    }
    let name: String = doc["client_name"].as_str().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(url.host_str().unwrap_or("MCP client")).chars().filter(|c| shown(*c)).take(80).collect();
    let uri = doc["client_uri"].as_str().filter(|u| u.starts_with("https://") && u.len() <= 500).map(str::to_string);
    Ok(crate::store::Client { id: id.to_string(), name, uri, redirect_uris, created_at: code_sql::now_ms(), last_used_at: None })
}

/// PKCE: BASE64URL(SHA256(verifier)) == challenge.
pub fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    use base64::Engine;
    use sha2::Digest;
    if !(43..=128).contains(&verifier.len()) {
        return false;
    }
    let digest = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    roll_auth::constant_eq(digest.as_bytes(), challenge.as_bytes())
}

fn token_answer(grant: &crate::store::Grant, access: String, refresh: String) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_MS / 1000,
            "refresh_token": refresh,
            "scope": "granted",
            "grant_id": grant.id,
        })),
    )
        .into_response()
}

async fn token(State(a): S, req: axum::extract::Request) -> Response {
    // The caller's address (a machine client's sign-in is audited with it), then the form.
    let ip = crate::evaluate::client_ip(&req, &a.config);
    let headers = req.headers().clone();
    let Ok(body) = axum::body::to_bytes(req.into_body(), 64 << 10).await else { return Error::OAuth("invalid_request", "the request is too large".into()).into_response() };
    let f: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    // A machine client, with no person (its id may be in a header or its assertion).
    let grant = get("grant_type");
    if matches!(grant.as_deref(), Some(crate::client_credentials::GRANT | crate::client_credentials::JWT_GRANT)) {
        let scope = if crate::client_credentials::read_only_scope(&f).unwrap_or(false) { "read-only" } else { "granted" };
        // A person's identity assertion from an enterprise identity provider (SEP-990, ID-JAG).
        if grant.as_deref() == Some(crate::client_credentials::JWT_GRANT) && get("assertion").as_deref().is_some_and(crate::enterprise::is_id_jag) {
            return match crate::enterprise::token(a, &headers, &f, ip).await {
                Ok((_, access, read_only)) => (
                    [(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")],
                    Json(json!({ "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL_MS / 1000, "scope": if read_only { "read-only" } else { "granted" } })),
                )
                    .into_response(),
                Err(e) => e.into_response(),
            };
        }
        let made = if grant.as_deref() == Some(crate::client_credentials::JWT_GRANT) {
            crate::client_credentials::federated(a, &f, ip).await
        } else {
            crate::client_credentials::token(a, &headers, &f, ip).await
        };
        return match made {
            // No refresh token: the client trades its credential again for the next one.
            Ok((_, access)) => (
                [(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")],
                Json(json!({ "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL_MS / 1000, "scope": scope })),
            )
                .into_response(),
            Err(e) => e.into_response(),
        };
    }
    let Some(client_id) = get("client_id") else { return Error::OAuth("invalid_client", "client_id is required".into()).into_response() };
    // roll's policy may list the clients that connect: checked before a code or refresh token is
    // spent (the store holds the grant to this client id), from what roll keeps (no fetch).
    let policy = a.policy_now();
    if !policy.oauth_clients.is_empty() {
        let (a2, id) = (a.clone(), client_id.clone());
        let known = tokio::task::spawn_blocking(move || a2.store.client(&id)).await;
        if !matches!(&known, Ok(Ok(Some(c))) if policy.allows_client(c)) {
            return Error::OAuth("unauthorized_client", crate::evaluate::CLIENT_NOT_ALLOWED.into()).into_response();
        }
    }
    let r = match get("grant_type").as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(redirect), Some(verifier)) = (get("code"), get("redirect_uri"), get("code_verifier")) else {
                return Error::OAuth("invalid_request", "code, redirect_uri and code_verifier are required".into()).into_response();
            };
            let resource = get("resource");
            // RFC 8707: the only resource roll's codes are for is its MCP server.
            if resource.as_deref().is_some_and(|r| r.trim_end_matches('/') != crate::oauth::resource(&a)) {
                return Error::OAuth("invalid_target", format!("resource must be {}", crate::oauth::resource(&a))).into_response();
            }
            tokio::task::spawn_blocking(move || a.store.exchange_code(&code, &client_id, &redirect, resource.as_deref(), |ch| pkce_ok(&verifier, ch))).await
        }
        Some("refresh_token") => {
            let Some(refresh) = get("refresh_token") else { return Error::OAuth("invalid_request", "refresh_token is required".into()).into_response() };
            tokio::task::spawn_blocking(move || a.store.refresh(&refresh, &client_id, a.config.refresh_grace.as_millis() as i64)).await
        }
        Some(DEVICE_GRANT) => {
            let Some(device_code) = get("device_code") else { return Error::OAuth("invalid_request", "device_code is required".into()).into_response() };
            let interval = a.config.device_interval.as_millis() as i64;
            tokio::task::spawn_blocking(move || a.store.poll_device(&device_code, &client_id, interval)).await
        }
        _ => return Error::OAuth("unsupported_grant_type", "grant_type is authorization_code, refresh_token or urn:ietf:params:oauth:grant-type:device_code".into()).into_response(),
    };
    match r {
        Ok(Ok((grant, access, refresh))) => token_answer(&grant, access, refresh),
        Ok(Err(e)) => e.into_response(),
        Err(e) => Error::Internal(e.to_string()).into_response(),
    }
}

/// RFC 7009: always 200, whether or not the token existed. A connection its client ended (`roll
/// logout`, an MCP client signing out) is in the audit log, as a person's revoke is.
async fn revoke(State(a): S, Form(f): Form<HashMap<String, String>>) -> Response {
    if let Some(t) = f.get("token").cloned() {
        let a2 = a.clone();
        if let Ok(Ok(Some(id))) = tokio::task::spawn_blocking(move || a2.store.revoke_secret(&t)).await {
            let a2 = a.clone();
            let name = tokio::task::spawn_blocking(move || a2.store.get(&id).map(|g| (g.id, g.name))).await;
            if let Ok(Ok((id, name))) = name {
                let e = crate::store::AuditEvent {
                    id: crate::new_id("aud"),
                    at: code_sql::now_ms(),
                    grant_id: Some(id),
                    grant_name: Some(name),
                    source: "access".into(),
                    tool: None,
                    method: "POST".into(),
                    route: "/v1/access/oauth/revoke".into(),
                    product: Some("access".into()),
                    action: Some("admin".into()),
                    resource: None,
                    decision: "revoke".into(),
                    approval: None,
                    status: 200,
                    args: Some(json!({ "by": "its client" })),
                    ip: None,
                    duration_ms: 0,
                    client: None,
                };
                if let Some(e) = a.store.queue_audit(e) {
                    let _ = tokio::task::spawn_blocking(move || a.store.audit(&e)).await;
                }
            }
        }
    }
    StatusCode::OK.into_response()
}

/// RFC 8628's grant type: a device's poll with its device code.
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Device requests from one network a minute at most (the endpoint is open, as registration is).
const DEVICES_PER_MINUTE: usize = 10;
/// Wrong user codes one caller may enter a minute: 34 bits of code against 10 guesses a minute.
const GUESSES_PER_MINUTE: usize = 10;

/// RFC 8628 3.1: a device asks to sign in, and gets the code its person enters on roll.
async fn device_authorization(State(a): S, req: axum::extract::Request) -> Response {
    let ip = crate::evaluate::client_ip(&req, &a.config);
    let network = ip.and_then(|i| crate::store::network_of(&i.to_string()));
    let admitted = a.device_starts.lock().unwrap_or_else(|e| e.into_inner()).admit(network.as_deref().unwrap_or("unknown"), code_sql::now_ms(), DEVICES_PER_MINUTE);
    if admitted.is_err() {
        return too_many("too many devices signing in from this network: try again in a minute");
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 4 << 10).await else { return Error::OAuth("invalid_request", "the request is too large".into()).into_response() };
    let f: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let Some(client_id) = f.get("client_id").filter(|c| !c.trim().is_empty()).cloned() else { return Error::OAuth("invalid_request", "client_id is required".into()).into_response() };
    // A scope is a hint for the page's preset: words only.
    let scope = f.get("scope").filter(|s| s.len() <= 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | ':'))).cloned();
    match known_client(&a, &client_id).await {
        // Only a client registered to sign devices in (no redirect): one registered with a
        // redirect is bound to it, and doesn't get tokens around it.
        Ok(Some(c)) if c.redirect_uris.is_empty() && !a.policy_now().allows_client(&c) => {
            return Error::OAuth("unauthorized_client", crate::evaluate::CLIENT_NOT_ALLOWED.into()).into_response();
        }
        Ok(Some(c)) if c.redirect_uris.is_empty() => {}
        Ok(Some(_)) => return Error::OAuth("unauthorized_client", "this client signs in with a redirect: register one without redirect_uris for the device flow".into()).into_response(),
        Ok(None) => return Error::OAuth("invalid_client", "unknown client_id: register first".into()).into_response(),
        Err(e) => return e.into_response(),
    }
    let a2 = a.clone();
    let made = tokio::task::spawn_blocking(move || a2.store.start_device(&client_id, scope.as_deref(), network.as_deref())).await;
    let (device_code, user_code) = match made {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return e.into_response(),
        Err(e) => return Error::Internal(e.to_string()).into_response(),
    };
    let shown = crate::store::show_user_code(&user_code);
    let page = format!("{}/oauth/device", a.config.web_url.trim_end_matches('/'));
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "device_code": device_code,
            "user_code": shown,
            "verification_uri": page,
            "verification_uri_complete": format!("{page}?code={shown}"),
            "expires_in": crate::store::DEVICE_TTL_MS / 1000,
            "interval": a.config.device_interval.as_secs().max(1),
        })),
    )
        .into_response()
}

/// The device request a signed-in person's code names, counting a wrong code against them. A
/// token never looks one up: only a person decides what a device may do.
async fn device_lookup(a: &Arc<Access>, ext: &Extensions, typed: &str) -> crate::Result<crate::store::DeviceRequest> {
    if let crate::api::Who::Grant(_) = crate::api::who(a, ext)? {
        return Err(Error::Forbidden("a token never signs a device in".into()));
    }
    // Past the limit, every code is refused, a right one too: otherwise guessing goes on at
    // request rate, and a 200 among the 429s says which guess was live.
    let key = crate::api::actor(ext);
    if a.device_guesses.lock().unwrap_or_else(|e| e.into_inner()).full(&key, code_sql::now_ms(), GUESSES_PER_MINUTE) {
        return Err(Error::TooMany("too many wrong codes: wait a minute".into()));
    }
    let found = match crate::store::normalize_user_code(typed) {
        Some(code) => {
            let a2 = a.clone();
            tokio::task::spawn_blocking(move || a2.store.device_request(&code)).await.map_err(|e| Error::Internal(e.to_string()))??
        }
        None => None,
    };
    if let Some(r) = found {
        return Ok(r);
    }
    let _ = a.device_guesses.lock().unwrap_or_else(|e| e.into_inner()).admit(&key, code_sql::now_ms(), GUESSES_PER_MINUTE);
    Err(Error::NotFound("no device waits for that code: check it on the device, or start again there".into()))
}

/// `GET /v1/access/oauth/devices/{code}`: what's asking, from where and since when, for the page.
pub async fn device_view(State(a): S, ext: Extensions, axum::extract::Path(code): axum::extract::Path<String>) -> crate::Result<Json<Value>> {
    let r = device_lookup(&a, &ext, &code).await?;
    let client = known_client(&a, &r.client_id).await?.ok_or_else(|| Error::NotFound("the device's client is gone".into()))?;
    let here = ext.get::<roll_auth::ClientIp>().and_then(|c| crate::store::network_of(&c.0.to_string()));
    Ok(Json(json!({
        "user_code": crate::store::show_user_code(&r.user_code),
        "client": { "id": client.id, "name": client.name, "uri": client.uri },
        "not_allowed": (!a.policy_now().allows_client(&client)).then_some(crate::evaluate::CLIENT_NOT_ALLOWED),
        "scope": r.scope,
        "network": r.network,
        // Asked from another network than this browser's: someone may have sent the code.
        "same_network": r.network.is_some() && r.network == here,
        "created_at": r.created_at,
        "expires_at": r.expires_at,
    })))
}

/// `POST /v1/access/oauth/devices/{code}`: the person allows the device what the body says (as
/// the consent screen's `Allowed`); its next poll takes the tokens.
pub async fn device_approve(State(a): S, ext: Extensions, axum::extract::Path(code): axum::extract::Path<String>, body: axum::body::Bytes) -> crate::Result<Response> {
    let allowed: Allowed = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let r = device_lookup(&a, &ext, &code).await?;
    let client = known_client(&a, &r.client_id).await?.ok_or_else(|| Error::NotFound("the device's client is gone".into()))?;
    if !a.policy_now().allows_client(&client) {
        return Err(Error::Forbidden(crate::evaluate::CLIENT_NOT_ALLOWED.into()));
    }
    let Consented { new, by, user, asker } = consented(&a, &ext, &client, &allowed)?;
    let waits = asker.is_some();
    let (client_name, asked_from) = (client.name.clone(), r.network.clone());
    let a2 = a.clone();
    let grant_id = tokio::task::spawn_blocking(move || a2.store.approve_device(&r.user_code, &client, &new, crate::store::Issuer { created_by: &by, user: user.as_deref() }))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    tell_consented(&a, &ext, asker, grant_id.clone(), crate::api::Added::Device { client: &client_name, asked_from: asked_from.as_deref() }).await;
    let mut res = Json(json!({ "grant_id": grant_id, "pending": waits })).into_response();
    res.extensions_mut().insert(crate::api::Changed(grant_id));
    Ok(res)
}

/// `DELETE /v1/access/oauth/devices/{code}`: the person refuses it.
pub async fn device_deny(State(a): S, ext: Extensions, axum::extract::Path(code): axum::extract::Path<String>) -> crate::Result<StatusCode> {
    let r = device_lookup(&a, &ext, &code).await?;
    let a2 = a.clone();
    tokio::task::spawn_blocking(move || a2.store.deny_device(&r.user_code)).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(StatusCode::NO_CONTENT)
}

/// Whether a client's name keeps `c`: not a control character, a backtick, or one a person can't
/// see but a model reads (zero-width and bidi controls, tag characters smuggling ASCII, variation
/// selectors hiding bytes).
fn shown(c: char) -> bool {
    !c.is_control()
        && c != '`'
        && !matches!(c as u32, 0x00AD | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2069 | 0xFE00..=0xFE0F | 0xFEFF | 0xE0000..=0xE007F | 0xE0100..=0xE01EF)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback http, https and app schemes register; browser-run schemes and remote http don't;
    /// PKCE checks the S256 challenge.
    #[test]
    fn redirects_and_pkce() {
        for (u, ok) in [
            ("http://localhost:33418/callback", true),
            ("http://127.0.0.1:5000/cb", true),
            ("https://claude.ai/api/mcp/auth_callback", true),
            ("cursor://anysphere.cursor-retrieval/oauth/callback", true),
            ("http://evil.example/cb", false),
            ("javascript:alert(1)", false),
            ("https://claude.ai/cb#frag", false),
        ] {
            assert_eq!(redirect_ok(u), ok, "{u}");
        }
        // RFC 7636 appendix B.
        assert!(pkce_ok("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!pkce_ok("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXx", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
    }
}
