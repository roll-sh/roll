//! `/v1/code/agents/{owner}/linear…` (docs/code/http-api.md, "Linear"): connect the owner's Linear
//! workspace (roll's OAuth app, or a personal API key), its settings, its webhook and recent
//! deliveries, and disconnect. Every owner route needs `Code::manages` (the owner or admin, not
//! its agents): it holds the workspace's credentials. `/v1/code/linear/oauth/callback` is where
//! Linear sends an install back (the state is the credential), and `/v1/code/linear/webhook…`
//! take deliveries (the signature is).

use super::client::{Auth, LinearError};
use super::store::{self, Connection, Kind, LinearSettings, NewConnection};
use super::webhook::{self, Endpoint};
use super::{SCOPES, steps};
use crate::api::{Body, Q};
use crate::model::Owner;
use crate::{Caller, Code, Error, Result, hash_token};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Code>>;

/// How long an install may take, from the button to Linear's redirect.
const STATE_TTL_MS: i64 = 15 * 60_000;
/// Deliveries the status shows.
const RECENT: usize = 20;

pub fn routes() -> Router<Arc<Code>> {
    Router::new()
        .route("/v1/code/agents/{owner}/linear", get(status).delete(disconnect))
        .route("/v1/code/agents/{owner}/linear/oauth", post(start_oauth))
        .route("/v1/code/agents/{owner}/linear/key", put(put_key))
        .route("/v1/code/agents/{owner}/linear/settings", put(put_settings))
        .route("/v1/code/agents/{owner}/linear/webhook/secret", put(put_hook_secret))
        .route("/v1/code/agents/{owner}/linear/teams", get(teams))
        .route("/v1/code/linear/oauth/callback", get(callback))
        .route("/v1/code/linear/webhook", post(app_webhook))
        .route("/v1/code/linear/webhook/{hook}", post(hook_webhook))
}

fn manage(code: &Code, caller: &Caller, owner: &str) -> Result<Owner> {
    let o = code.owner_or_404(owner)?;
    match caller {
        _ if Code::manages(caller, &o.id) => Ok(o),
        Caller::Anonymous => Err(Error::Unauthorized),
        c if c.agent().is_some() => Err(Error::Forbidden(format!("agent tokens can't manage {owner}'s Linear connection"))),
        _ => Err(Error::Forbidden(format!("not allowed to manage {owner}'s Linear connection"))),
    }
}

fn by(caller: &Caller) -> String {
    match caller {
        Caller::Owner { name, .. } => name.clone(),
        Caller::Admin => "admin".into(),
        _ => "admin".into(),
    }
}

/// A connection's own webhook address.
fn hook_url(code: &Code, hook_id: &str) -> String {
    format!("{}/v1/code/linear/webhook/{hook_id}", code.config.api_url.trim_end_matches('/'))
}

/// The connection as the API shows it: never a token or a secret.
fn status_json(code: &Code, owner_id: &str, c: Option<&Connection>) -> Result<Value> {
    let cfg = code.linear.config();
    let app = json!({
        "oauth": cfg.oauth().is_some(),
        "webhook": cfg.webhook_secret.is_some(),
        "webhook_url": format!("{}/v1/code/linear/webhook", code.config.api_url.trim_end_matches('/')),
        "redirect_uri": code.linear_redirect_uri(),
        "secret_key": code.ai.sealer.is_some(),
    });
    let Some(c) = c else { return Ok(json!({ "connected": false, "app": app, "connection": null, "deliveries": [] })) };
    let (mode, url) = match c.kind {
        Kind::OAuth => ("app", format!("{}/v1/code/linear/webhook", code.config.api_url.trim_end_matches('/'))),
        Kind::ApiKey if c.linear_webhook_id.is_some() => ("workspace", hook_url(code, &c.hook_id)),
        Kind::ApiKey => ("manual", hook_url(code, &c.hook_id)),
    };
    let secret_set = match c.kind {
        Kind::OAuth => cfg.webhook_secret.is_some(),
        Kind::ApiKey => c.hook_secret.is_some(),
    };
    let failing = c.rejected_at.is_some_and(|r| c.last_delivery_at.is_none_or(|l| r > l));
    let hook_status = if !secret_set {
        "needs_secret"
    } else if failing {
        "failing"
    } else if c.last_delivery_at.is_some() {
        "ok"
    } else {
        "waiting"
    };
    let deliveries = store::deliveries(code.db.sql(), owner_id, RECENT)?;
    Ok(json!({
        "connected": true,
        "app": app,
        "connection": {
            "kind": c.kind.as_str(),
            "workspace": { "id": c.organization_id, "name": c.organization_name, "url_key": c.url_key, "url": format!("https://linear.app/{}", c.url_key) },
            "user": { "id": c.user_id, "name": c.user_name },
            "agent": c.agent(),
            "scopes": c.scopes,
            "status": c.status,
            "error": c.error,
            "expires_at": c.expires_at,
            "created_at": c.created_at,
            "created_by": c.created_by,
            "settings": c.settings,
            "webhook": {
                "mode": mode,
                "url": url,
                "status": hook_status,
                "secret_hint": c.hook_hint,
                "last_delivery_at": c.last_delivery_at,
                "rejected_at": c.rejected_at,
                "rejected_why": c.rejected_why,
            },
        },
        "deliveries": deliveries,
    }))
}

/// `GET …/linear`.
async fn status(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = manage(&code, &caller, &owner)?;
    let c = store::connection(code.db.sql(), &o.id)?;
    Ok(Json(status_json(&code, &o.id, c.as_ref())?))
}

fn random_b64(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// A dashboard path to come back to: `/…`, not `//…` (another host), one line.
fn return_path(v: Option<&Value>) -> Result<String> {
    let p = match v {
        None | Some(Value::Null) => return Ok("/dashboard".into()),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(_) => return Err(Error::Invalid("return_to is a path".into())),
    };
    if !p.starts_with('/') || p.starts_with("//") || p.contains('\\') || p.len() > 500 || p.chars().any(char::is_control) {
        return Err(Error::Invalid("return_to is a path in the dashboard (/…)".into()));
    }
    Ok(p)
}

/// `POST …/linear/oauth {return_to?}` → `{url}`: where to send the owner to install roll's app
/// (as an agent: `actor=app`, assignable and mentionable).
async fn start_oauth(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<Value>> {
    let o = manage(&code, &caller, &owner)?;
    let cfg = code.linear.config();
    let Some((client_id, _)) = cfg.oauth() else {
        return Err(Error::Unprocessable("roll's Linear app isn't configured on this server (ROLL_LINEAR_CLIENT_ID, ROLL_LINEAR_CLIENT_SECRET): connect with an API key".into()));
    };
    let return_to = return_path(b.get("return_to"))?;
    let state = random_b64(24);
    let verifier = random_b64(32);
    let sealed = code.seal_linear(&o.id, "verifier", &verifier)?;
    let challenge = {
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
    };
    store::put_state(code.db.sql(), &hash_token(&state), &o.id, &by(&caller), &sealed, &return_to, code_sql::now_ms() + STATE_TTL_MS)?;
    let mut url = url::Url::parse(&cfg.authorize_url).map_err(|e| Error::Unprocessable(format!("ROLL_LINEAR_AUTHORIZE_URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &code.linear_redirect_uri())
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("state", &state)
        .append_pair("actor", "app")
        .append_pair("prompt", "consent")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(Json(json!({ "url": url.to_string() })))
}

const VIEWER: &str = "query RollViewer { viewer { id name displayName } organization { id name urlKey } }";

struct Who {
    user_id: String,
    user_name: String,
    org_id: String,
    org_name: String,
    url_key: String,
}

async fn who(code: &Code, auth: &Auth) -> std::result::Result<Who, LinearError> {
    let d = code.linear.graphql(auth, VIEWER, json!({})).await?;
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let w = Who {
        user_id: s(&d["viewer"]["id"]),
        user_name: d["viewer"]["displayName"].as_str().or(d["viewer"]["name"].as_str()).unwrap_or("roll").to_string(),
        org_id: s(&d["organization"]["id"]),
        org_name: s(&d["organization"]["name"]),
        url_key: s(&d["organization"]["urlKey"]),
    };
    if w.org_id.is_empty() || w.user_id.is_empty() {
        return Err(LinearError::Failed("Linear didn't say which workspace this is".into()));
    }
    Ok(w)
}

#[derive(Deserialize)]
struct Callback {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn back(code: &Code, path: &str, outcome: &str, reason: Option<&str>) -> Response {
    let mut to = format!("{}{path}", code.config.web_url.trim_end_matches('/'));
    to.push(if path.contains('?') { '&' } else { '?' });
    to.push_str(&format!("linear={outcome}"));
    if let Some(r) = reason {
        let r: String = r.chars().take(200).collect();
        to.push_str(&format!("&reason={}", percent_encoding::utf8_percent_encode(&r, percent_encoding::NON_ALPHANUMERIC)));
    }
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}

/// `GET /v1/code/linear/oauth/callback?code&state`: Linear's redirect after an install. The
/// state is taken once; the code is exchanged (with the PKCE verifier) for tokens, which are
/// sealed and stored; then back to the dashboard (`?linear=connected`, or `error` and a reason).
async fn callback(State(code): S, Q(q): Q<Callback>) -> Result<Response> {
    let Some(state) = q.state.filter(|s| !s.is_empty() && s.len() <= 200) else { return Err(Error::Invalid("no state".into())) };
    let Some(st) = store::take_state(code.db.sql(), &hash_token(&state))? else {
        return Ok(back(&code, "/dashboard", "error", Some("the install link expired or was used: start again")));
    };
    if let Some(e) = q.error {
        return Ok(back(&code, &st.return_to, "error", Some(&format!("Linear: {e}"))));
    }
    let Some(auth_code) = q.code.filter(|c| !c.is_empty() && c.len() <= 2000) else {
        return Ok(back(&code, &st.return_to, "error", Some("Linear sent no code")));
    };
    match install(&code, &st, &auth_code).await {
        Ok(()) => Ok(back(&code, &st.return_to, "connected", None)),
        Err(e) => Ok(back(&code, &st.return_to, "error", Some(&e))),
    }
}

async fn install(code: &Arc<Code>, st: &store::OAuthState, auth_code: &str) -> std::result::Result<(), String> {
    let cfg = code.linear.config();
    let (id, secret) = cfg.oauth().ok_or("roll's Linear app isn't configured any more")?;
    let verifier = code.open_linear(&st.owner_id, "verifier", &st.verifier).map_err(|e| e.to_string())?;
    let redirect = code.linear_redirect_uri();
    let t = code
        .linear
        .token(&[("grant_type", "authorization_code"), ("code", auth_code), ("redirect_uri", &redirect), ("client_id", id), ("client_secret", secret), ("code_verifier", &verifier)])
        .await
        .map_err(|e| match e {
            LinearError::Reauth(why) => format!("Linear refused the install: {why}"),
            e => e.to_string(),
        })?;
    let w = who(code, &Auth::Bearer(t.access_token.clone())).await.map_err(|e| e.to_string())?;
    let token = code.seal_linear(&st.owner_id, "token", &t.access_token).map_err(|e| e.to_string())?;
    let refresh = t.refresh_token.as_deref().map(|r| code.seal_linear(&st.owner_id, "refresh", r)).transpose().map_err(|e| e.to_string())?;
    let scopes = t.scopes();
    let scopes = if scopes.is_empty() { SCOPES.to_string() } else { scopes };
    let hook = crate::new_id("lwh");
    let n = NewConnection {
        owner_id: &st.owner_id,
        organization_id: &w.org_id,
        organization_name: &w.org_name,
        url_key: &w.url_key,
        kind: Kind::OAuth,
        token: &token,
        refresh: refresh.as_deref(),
        expires_at: t.expires_at(),
        scopes: &scopes,
        user_id: &w.user_id,
        user_name: &w.user_name,
        hook_id: &hook,
        hook_secret: None,
        hook_hint: None,
        linear_webhook_id: None,
        created_by: &st.created_by,
    };
    match store::put_connection(code.db.sql(), &n).map_err(|e| e.to_string())? {
        Ok(()) => {}
        Err(store::Taken) => return Err(format!("{} is connected to another roll owner", w.org_name)),
    }
    super::templates::install_defaults(code, &st.owner_id, &st.created_by).await;
    Ok(())
}

const WEBHOOK_CREATE: &str =
    "mutation RollWebhook($input: WebhookCreateInput!) { webhookCreate(input: $input) { success webhook { id enabled } } }";
const WEBHOOK_DELETE: &str = "mutation RollWebhookDelete($id: String!) { webhookDelete(id: $id) { success } }";

/// `PUT …/linear/key {key}`: connect with a personal API key. roll makes the workspace's webhook
/// itself when the key may (an admin's); otherwise the status says to make one by hand
/// (`webhook.mode: "manual"`) and paste its signing secret into `PUT …/linear/webhook/secret`.
async fn put_key(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<Value>> {
    let o = manage(&code, &caller, &owner)?;
    let key = b.get("key").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    if key.is_empty() || key.len() > 200 || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Invalid("key is a Linear API key (lin_api_…)".into()));
    }
    // Sealing works before anything is asked of Linear.
    let sealed = code.seal_linear(&o.id, "token", &key)?;
    let auth = Auth::Key(key);
    let w = who(&code, &auth).await.map_err(|e| match e {
        LinearError::Unauthorized => Error::Invalid("Linear doesn't take this API key".into()),
        e => e.into(),
    })?;
    let hook = crate::new_id("lwh");
    let secret = crate::agents::api::random_hex(32);
    let made = code
        .linear
        .graphql(&auth, WEBHOOK_CREATE, json!({ "input": { "url": hook_url(&code, &hook), "resourceTypes": ["Issue", "Comment"], "allPublicTeams": true, "secret": secret, "label": "roll" } }))
        .await;
    let (webhook_id, hook_secret, hint) = match made {
        Ok(d) if d["webhookCreate"]["webhook"]["id"].is_string() => {
            let id = d["webhookCreate"]["webhook"]["id"].as_str().unwrap_or_default().to_string();
            (Some(id), Some(code.seal_linear(&o.id, "hook", &secret)?), Some(secret[secret.len() - 4..].to_string()))
        }
        Ok(_) | Err(_) => (None, None, None),
    };
    let n = NewConnection {
        owner_id: &o.id,
        organization_id: &w.org_id,
        organization_name: &w.org_name,
        url_key: &w.url_key,
        kind: Kind::ApiKey,
        token: &sealed,
        refresh: None,
        expires_at: None,
        scopes: "read,write",
        user_id: &w.user_id,
        user_name: &w.user_name,
        hook_id: &hook,
        hook_secret: hook_secret.as_deref(),
        hook_hint: hint.as_deref(),
        linear_webhook_id: webhook_id.as_deref(),
        created_by: &by(&caller),
    };
    if store::put_connection(code.db.sql(), &n)?.is_err() {
        if let Some(id) = &webhook_id {
            let _ = code.linear.graphql(&auth, WEBHOOK_DELETE, json!({ "id": id })).await;
        }
        return Err(Error::Conflict(format!("{} is connected to another roll owner", w.org_name)));
    }
    let c = store::connection(code.db.sql(), &o.id)?;
    Ok(Json(status_json(&code, &o.id, c.as_ref())?))
}

/// `PUT …/linear/settings LinearSettings`: which repo each team's issues are in.
async fn put_settings(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<Value>> {
    let o = manage(&code, &caller, &owner)?;
    let s: LinearSettings = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("settings: {e}")))?;
    if s.repos.len() > 200 {
        return Err(Error::Invalid("at most 200 teams".into()));
    }
    for (team, repo) in s.repos.iter().map(|(t, r)| (t.as_str(), r.as_str())).chain(s.default_repo.as_deref().map(|r| ("default_repo", r))) {
        if team.trim().is_empty() || team.len() > 100 {
            return Err(Error::Invalid("a team is its key, name or id".into()));
        }
        let name = repo.rsplit('/').next().unwrap_or_default();
        if repo.trim().is_empty() || repo.len() > 200 || repo.matches('/').count() > 1 || name.is_empty() {
            return Err(Error::Invalid(format!("{team}: {repo:?} isn't a repo (`repo` or `owner/repo`)")));
        }
    }
    if !store::set_settings(code.db.sql(), &o.id, &s)? {
        return Err(Error::NotFound("Linear isn't connected".into()));
    }
    let c = store::connection(code.db.sql(), &o.id)?;
    Ok(Json(status_json(&code, &o.id, c.as_ref())?))
}

/// `PUT …/linear/webhook/secret {secret}`: the signing secret of a webhook made by hand in Linear
/// (API keys). Sealed; never returned.
async fn put_hook_secret(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<Value>> {
    let o = manage(&code, &caller, &owner)?;
    let secret = b.get("secret").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if secret.len() < 8 || secret.len() > 200 || secret.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Invalid("secret is the webhook's signing secret from Linear".into()));
    }
    let Some(c) = store::connection(code.db.sql(), &o.id)? else { return Err(Error::NotFound("Linear isn't connected".into())) };
    if c.kind == Kind::OAuth {
        return Err(Error::Conflict("roll's Linear app signs its webhooks with the server's secret; there's nothing to set".into()));
    }
    let sealed = code.seal_linear(&o.id, "hook", secret)?;
    store::set_hook_secret(code.db.sql(), &o.id, &sealed, &secret[secret.len() - 4..])?;
    let c = store::connection(code.db.sql(), &o.id)?;
    Ok(Json(status_json(&code, &o.id, c.as_ref())?))
}

/// `GET …/linear/teams`: the workspace's teams (for the settings and the builder).
async fn teams(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = manage(&code, &caller, &owner)?;
    let ts = steps::teams(&code, &o.id).await?;
    Ok(Json(json!({ "teams": ts.iter().map(|t| json!({ "id": t["id"], "key": t["key"], "name": t["name"] })).collect::<Vec<_>>() })))
}

/// `DELETE …/linear`: revoke roll's token (OAuth) or remove the webhook roll made (API key), best
/// effort, and forget the connection.
async fn disconnect(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<StatusCode> {
    let o = manage(&code, &caller, &owner)?;
    let Some(c) = store::connection(code.db.sql(), &o.id)? else { return Err(Error::NotFound("Linear isn't connected".into())) };
    if let Ok(t) = code.open_linear(&o.id, "token", &c.token) {
        match c.kind {
            Kind::OAuth => code.linear.revoke(&t).await,
            Kind::ApiKey => {
                if let Some(id) = &c.linear_webhook_id
                    && let Err(e) = code.linear.graphql(&Auth::Key(t), WEBHOOK_DELETE, json!({ "id": id })).await
                {
                    tracing::warn!(error = %e, "removing roll's Linear webhook");
                }
            }
        }
    }
    store::delete_connection(code.db.sql(), &o.id)?;
    Ok(StatusCode::NO_CONTENT)
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

async fn deliver(code: Arc<Code>, at: Endpoint<'_>, headers: HeaderMap, body: axum::body::Bytes) -> Result<Json<Value>> {
    if body.len() > webhook::MAX_BODY {
        return Err(Error::Invalid(format!("a Linear delivery is at most {} KiB", webhook::MAX_BODY >> 10)));
    }
    let delivery = header_str(&headers, "linear-delivery").unwrap_or_default();
    let r = webhook::receive(&code, at, delivery, header_str(&headers, "linear-signature"), &body)?;
    Ok(Json(json!({ "recorded": r.recorded, "duplicates": r.duplicates, "ignored": r.ignored })))
}

/// `POST /v1/code/linear/webhook`: roll's OAuth app's deliveries.
async fn app_webhook(State(code): S, headers: HeaderMap, body: axum::body::Bytes) -> Result<Json<Value>> {
    deliver(code, Endpoint::App, headers, body).await
}

/// `POST /v1/code/linear/webhook/{hook}`: a connection's own webhook.
async fn hook_webhook(State(code): S, Path(hook): Path<String>, headers: HeaderMap, body: axum::body::Bytes) -> Result<Json<Value>> {
    deliver(code.clone(), Endpoint::Hook(&hook), headers, body).await
}
