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
use axum::http::{Extensions, HeaderMap, StatusCode, header};
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
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": SCOPES,
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
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

fn reg_error(code: &'static str, desc: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": code, "error_description": desc }))).into_response()
}

/// Registrations from one network (/24, /48) a minute at most, on a node: registration is open.
const REGISTER_PER_MINUTE: u32 = 20;

/// RFC 7591: open registration of public clients (PKCE, no secret).
async fn register(State(a): S, req: axum::extract::Request) -> Response {
    let ip = crate::evaluate::client_ip(&req, a.config.trust_forwarded_for);
    let network = ip.and_then(|i| crate::store::network_of(&i.to_string())).unwrap_or_else(|| "unknown".into());
    {
        let now = code_sql::now_ms() / 60_000;
        let mut seen = a.registrations.lock().unwrap_or_else(|e| e.into_inner());
        // A new minute starts the counts again; within one, the map holds at most 10,000 networks
        // and refuses the rest (one sweep a minute, never a scan per request).
        if seen.get("").is_none_or(|(minute, _)| *minute != now) {
            seen.clear();
            seen.insert(String::new(), (now, 0));
        }
        if !seen.contains_key(&network) && seen.len() > 10_000 {
            return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], Json(json!({ "error": "too_many_requests", "error_description": "too many registrations lately: try again in a minute" }))).into_response();
        }
        let (minute, n) = seen.entry(network).or_insert((now, 0));
        if *minute != now {
            (*minute, *n) = (now, 0);
        }
        if *n >= REGISTER_PER_MINUTE {
            return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], Json(json!({ "error": "too_many_requests", "error_description": "too many registrations from this network: try again in a minute" }))).into_response();
        }
        *n += 1;
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), (16 << 10) + 1).await else { return reg_error("invalid_client_metadata", "the registration is too large") };
    if body.len() > 16 << 10 {
        return reg_error("invalid_client_metadata", "the registration is too large");
    }
    let Ok(r) = serde_json::from_slice::<Registration>(&body) else { return reg_error("invalid_client_metadata", "the body must be JSON client metadata") };
    if r.redirect_uris.is_empty() || r.redirect_uris.len() > 10 || !r.redirect_uris.iter().all(|u| redirect_ok(u)) {
        return reg_error("invalid_redirect_uri", "redirect_uris: 1-10 URIs, each https, http on localhost, or an app scheme");
    }
    if r.token_endpoint_auth_method.as_deref().is_some_and(|m| m != "none") {
        return reg_error("invalid_client_metadata", "only public clients (token_endpoint_auth_method \"none\", with PKCE) can register");
    }
    if r.grant_types.as_ref().is_some_and(|g| g.iter().any(|t| t != "authorization_code" && t != "refresh_token")) {
        return reg_error("invalid_client_metadata", "grant_types: authorization_code and refresh_token");
    }
    // A name as consent and agents show it: no control or invisible formatting characters (bidi
    // overrides, zero-width), no backticks, at most 64 (what a token's name may be).
    let invisible = |c: char| matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '`');
    let cleaned: String = r.client_name.as_deref().unwrap_or("").chars().filter(|c| !c.is_control() && !invisible(*c)).take(64).collect();
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
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
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

/// The consent screen's answer, from the operator or a signed-in user (whose connection is
/// their own: it acts as them): a code for the client, in the redirect the dashboard sends the
/// browser to.
pub async fn authorize(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    let user = match crate::api::who(&a, &ext)? {
        // A signed-in admin's connection is theirs (it acts as the operator while they're one).
        crate::api::Who::Operator => ext.get::<roll_auth::SignedIn>().map(|u| u.user.clone()),
        crate::api::Who::User(u) => Some(u.user),
        crate::api::Who::Grant(_) => return Err(Error::Forbidden("a token never connects an MCP client".into())),
    };
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
    let a2 = a.clone();
    let cid = c.client_id.clone();
    let client = known_client(&a2, &cid).await?.ok_or_else(|| Error::BadRequest("unknown client_id".into()))?;
    if !client.redirect_uris.contains(&c.redirect_uri) {
        return Err(Error::BadRequest("redirect_uri isn't one the client registered".into()));
    }
    a.check_policy(c.expires_at)?;
    let new = NewGrant { name: c.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| client.name.clone()), rules: c.rules.clone(), expires_at: c.expires_at, ip_allow: c.ip_allow.clone(), rate_per_minute: c.rate_per_minute, ask_otherwise: c.ask_otherwise };
    let (a2, cl, uri, ch, res, by) = (a.clone(), client.clone(), c.redirect_uri.clone(), c.code_challenge.clone(), Some(resource(&a)), crate::api::actor(&ext));
    let code = tokio::task::spawn_blocking(move || a2.store.issue_code(&cl, &new, crate::store::Issuer { created_by: &by, user: user.as_deref() }, &uri, &ch, res.as_deref()))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    let mut redirect = url::Url::parse(&c.redirect_uri).map_err(|_| Error::BadRequest("redirect_uri isn't a URL".into()))?;
    {
        let mut q = redirect.query_pairs_mut();
        q.append_pair("code", &code);
        if let Some(s) = &c.state {
            q.append_pair("state", s);
        }
        q.append_pair("iss", &api(&a));
    }
    Ok(Json(json!({ "redirect": redirect.to_string() })))
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
    let name: String = doc["client_name"].as_str().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(url.host_str().unwrap_or("MCP client")).chars().filter(|c| !c.is_control()).take(80).collect();
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

async fn token(State(a): S, headers: HeaderMap, Form(f): Form<HashMap<String, String>>) -> Response {
    let _ = headers;
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let Some(client_id) = get("client_id") else { return Error::OAuth("invalid_client", "client_id is required".into()).into_response() };
    let r = match get("grant_type").as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(redirect), Some(verifier)) = (get("code"), get("redirect_uri"), get("code_verifier")) else {
                return Error::OAuth("invalid_request", "code, redirect_uri and code_verifier are required".into()).into_response();
            };
            let resource = get("resource");
            tokio::task::spawn_blocking(move || a.store.exchange_code(&code, &client_id, &redirect, resource.as_deref(), |ch| pkce_ok(&verifier, ch))).await
        }
        Some("refresh_token") => {
            let Some(refresh) = get("refresh_token") else { return Error::OAuth("invalid_request", "refresh_token is required".into()).into_response() };
            tokio::task::spawn_blocking(move || a.store.refresh(&refresh, &client_id, a.config.refresh_grace.as_millis() as i64)).await
        }
        _ => return Error::OAuth("unsupported_grant_type", "grant_type is authorization_code or refresh_token".into()).into_response(),
    };
    match r {
        Ok(Ok((grant, access, refresh))) => token_answer(&grant, access, refresh),
        Ok(Err(e)) => e.into_response(),
        Err(e) => Error::Internal(e.to_string()).into_response(),
    }
}

/// RFC 7009: always 200, whether or not the token existed.
async fn revoke(State(a): S, Form(f): Form<HashMap<String, String>>) -> Response {
    if let Some(t) = f.get("token").cloned() {
        let _ = tokio::task::spawn_blocking(move || a.store.revoke_secret(&t)).await;
    }
    StatusCode::OK.into_response()
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
