//! `/v1/accounts` (docs/accounts.md, "Routes"): JSON in and out, errors as
//! `{"error": "...", "code"?: "..."}`.
//!
//! rolld's gate has already resolved the session cookie (`SignedIn` or `SecondFactorOwed` in the
//! request's extensions) and checked CSRF on writes; the operator's token or an admin's session
//! carries `Operator`. These routes are `Access::Product` there: each handler decides who it
//! serves.

use crate::oauth::{Finished, Provider};
use crate::{Accounts, Error, Issued, Result, SECOND_FACTOR_TTL_MS, SESSION_TTL_MS};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use bytes::Bytes;
use roll_auth::{Operator, SecondFactorOwed, SignedIn};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Accounts>>;
type Me = Option<Extension<SignedIn>>;
type Owed = Option<Extension<SecondFactorOwed>>;
type Op = Option<Extension<Operator>>;
/// The client's address, as the audit layer read it (its trusted proxies).
type Ip = Option<Extension<roll_auth::ClientIp>>;
use crate::audit::Who;

pub fn router(accounts: Arc<Accounts>) -> Router {
    Router::new()
        .route("/v1/accounts", get(info))
        .route("/v1/accounts/session", get(session))
        .route("/v1/accounts/signout", post(sign_out))
        .route("/v1/accounts/email/start", post(email_start))
        .route("/v1/accounts/email/verify", post(email_verify))
        .route("/v1/accounts/oauth/{provider}/start", post(oauth_start))
        .route("/v1/accounts/oauth/{provider}/callback", get(oauth_callback))
        .route("/v1/accounts/signin/totp", post(signin_totp))
        .route("/v1/accounts/signin/recovery", post(signin_recovery))
        .route("/v1/accounts/signin/passkey/options", post(signin_passkey_options))
        .route("/v1/accounts/signin/passkey", post(signin_passkey))
        .route("/v1/accounts/signin/reset-request", post(request_reset))
        .route("/v1/accounts/reset-requests", get(list_resets))
        .route("/v1/accounts/passkey-policy", get(passkey_policy).put(set_passkey_policy))
        .route("/v1/accounts/policy", get(account_policy).put(set_account_policy))
        .route("/v1/accounts/network-policy", get(network_policy).put(set_network_policy))
        .route("/v1/accounts/reset-requests/{id}/approve", post(approve_reset))
        .route("/v1/accounts/reset-requests/{id}/deny", post(deny_reset))
        .route("/v1/accounts/me", get(me).patch(update_me).delete(delete_me))
        .route("/v1/accounts/me/email", post(change_email))
        .route("/v1/accounts/me/email/confirm", post(confirm_email))
        .route("/v1/accounts/me/sessions", get(my_sessions).delete(end_other_sessions))
        .route("/v1/accounts/me/activity", get(my_activity))
        .route("/v1/accounts/me/sessions/{id}", delete(revoke_my_session))
        .route("/v1/accounts/me/identities/{provider}", delete(unlink))
        .route("/v1/accounts/me/totp", post(totp_enrol).delete(totp_disable))
        .route("/v1/accounts/me/totp/confirm", post(totp_confirm))
        .route("/v1/accounts/me/recovery-codes", post(recovery_codes))
        .route("/v1/accounts/me/passkeys", get(my_passkeys).post(add_passkey))
        .route("/v1/accounts/me/reauth", get(reauth_info).post(reauth))
        .route("/v1/accounts/me/export", post(export_me))
        .route("/v1/accounts/me/reauth/email", post(reauth_email))
        .route("/v1/accounts/me/reauth/passkey/options", post(reauth_passkey_options))
        .route("/v1/accounts/me/passkeys/options", post(passkey_options))
        .route("/v1/accounts/me/passkeys/{id}", axum::routing::patch(rename_passkey).delete(remove_passkey))
        .route("/v1/accounts/invites", get(list_invites).post(create_invite))
        .route("/v1/accounts/invites/peek", post(peek_invite))
        .route("/v1/accounts/invites/{id}", delete(revoke_invite))
        .route("/v1/accounts/invites/{id}/resend", post(resend_invite))
        .route("/v1/accounts/users", get(list_users))
        .route("/v1/accounts/users/{id}", axum::routing::patch(update_user))
        .route("/v1/accounts/users/{id}/sessions", get(user_sessions).delete(revoke_user_sessions))
        .route("/v1/accounts/users/{id}/sessions/{session}", delete(revoke_user_session))
        .route("/v1/accounts/users/{id}/activity", get(user_activity))
        .route("/v1/accounts/sso", get(list_sso).post(add_sso))
        .route("/v1/accounts/sso/start", post(sso_start))
        .route("/v1/accounts/sso/{id}", axum::routing::patch(update_sso).delete(remove_sso))
        .route("/v1/accounts/sso/{id}/callback", get(sso_callback))
        .route("/v1/accounts/sso/{id}/verify", post(verify_sso))
        .route("/v1/accounts/sso/{id}/backchannel-logout", post(backchannel_logout))
        .route("/v1/accounts/sso/{id}/scim", post(scim_token).delete(remove_scim_token))
        .merge(crate::scim::routes())
        .route_layer(axum::middleware::from_fn(json_only))
        .route_layer(axum::middleware::from_fn_with_state(accounts.clone(), crate::audit::layer))
        .with_state(accounts)
}

/// A browser's write here is JSON (docs/accounts.md, "CSRF"). A form on another site can post
/// any text as its body, and the browser keeps the cookie its answer sets: without this, a page
/// could post the attacker's own email code and sign the visitor in to the attacker's account.
/// `application/json` from another origin needs a CORS preflight, which only the dashboard
/// passes. Calls with a token aren't a browser's and are left alone.
async fn json_only(req: Request, next: Next) -> Response {
    let json = |v: &HeaderValue| v.to_str().ok().and_then(|s| s.split(';').next()).is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
    let h = req.headers();
    // OIDC Back-Channel Logout posts a form, by spec: it reads no cookie and sets none, and only a
    // token the provider's key signed does anything, which no other site can make.
    let logout = req.uri().path().starts_with("/v1/accounts/sso/") && req.uri().path().ends_with("/backchannel-logout");
    let form = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && !logout
        && !h.contains_key(header::AUTHORIZATION)
        && h.get(header::CONTENT_TYPE).is_some_and(|v| !json(v));
    if form {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({ "error": "send the body as JSON (Content-Type: application/json)" }))).into_response();
    }
    next.run(req).await
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Error::BadRequest(_, code) => (StatusCode::BAD_REQUEST, *code),
            Error::Unauthorized(_) => (StatusCode::UNAUTHORIZED, Some("unauthorized")),
            Error::Forbidden(_, code) => (StatusCode::FORBIDDEN, Some(*code)),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, None),
            Error::Conflict(_) => (StatusCode::CONFLICT, None),
            Error::Unavailable(_, code) => (StatusCode::SERVICE_UNAVAILABLE, Some(*code)),
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
        };
        let msg = match &self {
            Error::Db(_) | Error::Internal(_) => {
                tracing::warn!(error = %self, "accounts: request failed");
                "internal error".to_string()
            }
            e => e.to_string(),
        };
        let mut body = json!({ "error": msg });
        if let Some(c) = code {
            body["code"] = c.into();
        }
        (status, Json(body)).into_response()
    }
}

/// Run `f` off the async runtime: the store blocks on its database (and argon2 on the CPU).
async fn blocking<T: Send + 'static>(a: &Arc<Accounts>, f: impl FnOnce(&Accounts) -> Result<T> + Send + 'static) -> Result<T> {
    let a = a.clone();
    tokio::task::spawn_blocking(move || f(&a)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// Tell the other admins what `by` just changed (alerts.rs). A failed lookup is logged: the change
/// itself stands.
async fn alert(a: &Arc<Accounts>, by: String, what: String) {
    match blocking(a, move |a| a.alert_mails(&by, &what)).await {
        Ok(mails) => mails.into_iter().for_each(|m| a.send_later(m, "admin alert")),
        Err(e) => tracing::warn!(error = %e, "accounts: an admin alert failed"),
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// The body as JSON, a 400 in the API's shape if it isn't.
fn body<T: DeserializeOwned>(b: &Bytes) -> Result<T> {
    let b: &[u8] = if b.iter().all(u8::is_ascii_whitespace) { b"{}" } else { b };
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}"), None))
}

fn signed_in(me: Me) -> Result<SignedIn> {
    me.map(|Extension(u)| u).ok_or_else(Error::signed_out)
}

/// Sudo mode (docs/accounts.md): a change to how an account is reached needs a recent proof.
async fn sudo(a: &Arc<Accounts>, u: &SignedIn) -> Result<()> {
    let s = u.session.clone();
    blocking(a, move |a| a.fresh(&s)).await
}

/// An admin's change to who's an admin or how people sign in: an admin's session must be fresh
/// (the operator's token has no session).
async fn admin_sudo(a: &Arc<Accounts>, me: &Me, op: &Op) -> Result<String> {
    let by = admin(a, me, op)?;
    if let Some(Extension(u)) = me {
        sudo(a, u).await?;
    }
    Ok(by)
}

/// Who may use the admin routes: an admin's session (by their id), the operator's token, or
/// anyone on an open daemon.
fn admin(a: &Accounts, me: &Me, op: &Op) -> Result<String> {
    match me {
        Some(Extension(u)) if u.admin => Ok(u.user.clone()),
        Some(_) => Err(Error::Forbidden("only roll's admins can do this".into(), "admin_only")),
        None if op.is_some() || a.config.open => Ok("operator".into()),
        None => Err(Error::Unauthorized("this needs an admin's session or the operator's token".into())),
    }
}

fn user_agent(h: &HeaderMap) -> String {
    let ua = h.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    ua.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// `Set-Cookie` for `name`: httpOnly, `SameSite=Lax`, the whole API, `Secure` over HTTPS.
fn cookie(a: &Accounts, name: &str, value: &str, max_age_ms: i64) -> HeaderValue {
    let secure = if a.config.secure() { "; Secure" } else { "" };
    let v = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}", max_age_ms.max(0) / 1000);
    HeaderValue::from_str(&v).expect("cookie characters")
}

fn with_cookies(mut resp: Response, cookies: impl IntoIterator<Item = HeaderValue>) -> Response {
    for c in cookies {
        resp.headers_mut().append(header::SET_COOKIE, c);
    }
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// The answer that describes a session: `SignedIn`, or `MfaRequired` while a factor is owed.
fn session_body(a: &Accounts, issued_token: &str, user: &str, session: &str, second_factor: bool) -> Result<Value> {
    let csrf = roll_auth::csrf_token(issued_token);
    if second_factor {
        return Ok(json!({ "status": "mfa_required", "methods": second_methods(a, user), "csrf": csrf }));
    }
    let missing = a.missing(user, session)?;
    Ok(json!({
        "status": "signed_in", "user": a.user(user)?, "session": a.session_info(session)?, "csrf": csrf,
        "second_factor_required": missing == Some(roll_auth::Missing::SecondFactor),
        "passkey_required": missing == Some(roll_auth::Missing::Passkey),
    }))
}

/// How a person owing a second factor can give it: TOTP, a recovery code, and a passkey if they
/// have one.
fn second_methods(a: &Accounts, user: &str) -> Value {
    let passkey = a.webauthn_site().is_ok() && a.passkeys(user).is_ok_and(|p| !p.is_empty());
    if passkey { json!(["totp", "recovery", "passkey"]) } else { json!(["totp", "recovery"]) }
}

/// A new session's answer, with its cookie.
fn signed_in_response(a: &Accounts, user: &str, issued: Issued) -> Result<Response> {
    let body = session_body(a, &issued.token, user, &issued.id, issued.second_factor)?;
    let ttl = if issued.second_factor { SECOND_FACTOR_TTL_MS } else { SESSION_TTL_MS };
    Ok(with_cookies(Json(body).into_response(), [cookie(a, a.config.session_cookie(), &issued.token, ttl)]))
}

// ── public ──

async fn info(State(a): S) -> Json<Value> {
    let sso = blocking(&a, |a| a.sso_connections()).await.is_ok_and(|c| !c.is_empty());
    let passkey = a.webauthn_site().is_ok();
    Json(json!({ "providers": { "email": a.mail_on(), "github": a.config.github.is_some(), "google": a.config.google.is_some(), "sso": sso, "passkey": passkey } }))
}

async fn session(State(a): S, me: Me, owed: Owed, headers: HeaderMap) -> Result<Response> {
    let token = roll_auth::cookie(&headers, a.config.session_cookie()).map(str::to_string);
    let body = match (me, owed, &token) {
        (Some(Extension(u)), _, Some(t)) => {
            let t = t.clone();
            blocking(&a, move |a| session_body(a, &t, &u.user, &u.session, false)).await?
        }
        (None, Some(Extension(o)), Some(t)) => {
            let (t, user) = (t.clone(), o.user.clone());
            let methods = blocking(&a, move |a| Ok(second_methods(a, &user))).await?;
            json!({ "status": "mfa_required", "methods": methods, "csrf": roll_auth::csrf_token(&t) })
        }
        _ => {
            let resp = Json(json!({ "status": "anonymous" })).into_response();
            // A cookie that no longer stands for a session goes. With an `Authorization` header the
            // gate didn't look at the cookie, so it may still be good.
            let clear = (token.is_some() && !headers.contains_key(header::AUTHORIZATION)).then(|| cookie(&a, a.config.session_cookie(), "", 0));
            return Ok(with_cookies(resp, clear));
        }
    };
    Ok(with_cookies(Json(body).into_response(), None))
}

async fn sign_out(State(a): S, me: Me, owed: Owed) -> Result<Response> {
    let session = me.map(|Extension(u)| u.session).or(owed.map(|Extension(o)| o.session));
    if let Some(s) = session {
        blocking(&a, move |a| a.sign_out(&s)).await?;
    }
    Ok(with_cookies(StatusCode::NO_CONTENT.into_response(), [cookie(&a, a.config.session_cookie(), "", 0)]))
}

#[derive(Deserialize)]
struct EmailStart {
    email: String,
    invite: Option<String>,
}

async fn email_start(State(a): S, b: Bytes) -> Result<Response> {
    if !a.mail_on() {
        return Err(Error::Unavailable("email sign-in isn't configured on this server".into(), "email_off"));
    }
    let EmailStart { email, invite } = body(&b)?;
    let email = crate::normalize_email(&email)?;
    // A domain whose SSO connection enforces it signs in only there (its domain alone decides:
    // nothing about whether the address has an account).
    let e2 = email.clone();
    if let Some(name) = blocking(&a, move |a| a.sso_required_for(&e2)).await? {
        return Err(Error::Forbidden(format!("sign in with {name}: your organization requires it"), "sso_required"));
    }
    let challenge = crate::new_id("chl");
    // Whether the address has an account decides only work done after the answer, off the
    // request (lookups, the throttle, argon2, the write, the mail): the answer's timing can't
    // say which it was.
    let (a2, id) = (a.clone(), challenge.clone());
    tokio::spawn(async move {
        match blocking(&a2, move |a| a.email_start(&id, &email, invite.as_deref())).await {
            Ok(Some(mail)) => a2.send_later(mail, "sign-in code"),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "accounts: starting an email sign-in failed"),
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "challenge": challenge }))).into_response())
}

#[derive(Deserialize)]
struct Verify {
    challenge: String,
    code: String,
}

async fn email_verify(State(a): S, ip: Ip, headers: HeaderMap, b: Bytes) -> Result<Response> {
    let Verify { challenge, code } = body(&b)?;
    let ua = user_agent(&headers);
    let who = Who::of(ip, &headers);
    blocking(&a, move |a| {
        let (user, made) = a.email_verify(&challenge, &code).inspect_err(|e| a.record_code_refused(a.code_owner(&challenge).as_deref(), "email", e, &who))?;
        if made {
            a.record_new(&user, "invite", "email", &who);
        }
        // Past the code, the person is known: a refusal now (disabled, SSO required) is theirs.
        let issued = a.issue(&user, &ua, "email").inspect_err(|e| a.record_sign_in(Some(&user), "email", Some(e), false, &who))?;
        a.record_sign_in(Some(&user), "email", None, issued.second_factor, &who);
        signed_in_response(a, &user, issued)
    })
    .await
}

#[derive(Deserialize)]
struct OAuthStart {
    #[serde(default)]
    intent: Option<String>,
    invite: Option<String>,
    return_to: Option<String>,
}

fn provider(name: &str) -> Result<Provider> {
    Provider::parse(name).ok_or_else(|| Error::NotFound(format!("no sign-in provider {name:?}")))
}

async fn oauth_start(State(a): S, me: Me, Path(p): Path<String>, b: Bytes) -> Result<Response> {
    let p = provider(&p)?;
    let OAuthStart { intent, invite, return_to } = body(&b)?;
    let link_user = match intent.as_deref().unwrap_or("signin") {
        "signin" => None,
        "link" => Some(signed_in(me)?.user),
        other => return Err(Error::BadRequest(format!("intent is signin or link, not {other}"), None)),
    };
    let started = blocking(&a, move |a| a.oauth_start(p, link_user.as_deref(), invite.as_deref(), return_to.as_deref())).await?;
    let resp = Json(json!({ "url": started.url })).into_response();
    Ok(with_cookies(resp, [cookie(&a, a.config.oauth_cookie(), &started.browser, 10 * 60_000)]))
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn oauth_callback(State(a): S, me: Me, ip: Ip, Path(p): Path<String>, Query(q): Query<Callback>, headers: HeaderMap) -> Response {
    let who = Who::of(ip, &headers).person(me);
    // No provider, or no code (the person said no at the provider): no sign-in happened to record.
    let Some(p) = Provider::parse(&p) else { return finish_redirect(&a, &headers, Err(Error::NotFound("no such provider".into())), "/signin".into(), false, "", &who).await };
    let (Some(code), Some(state)) = (q.code, q.state) else {
        let _ = q.error;
        return finish_redirect(&a, &headers, Err(Error::BadRequest("the provider".into(), Some("provider"))), "/signin".into(), false, "", &who).await;
    };
    let browser = roll_auth::cookie(&headers, a.config.oauth_cookie()).map(str::to_string);
    let (done, back) = a.oauth_callback(p, &code, &state, browser).await;
    finish_redirect(&a, &headers, done, back, false, p.as_str(), &who).await
}

/// Where a provider's callback sends the browser: the session it signed in (and its cookie), or
/// back with `?error=`. `sso`: a session for a person their SSO provider signed in.
async fn finish_redirect(a: &Arc<Accounts>, headers: &HeaderMap, done: Result<Finished>, back: String, sso: bool, method: &str, who: &Who) -> Response {
    // In the audit log: a link (the signed-in person's), or a sign-in's end. `method` empty: no
    // sign-in happened (no code came back), nothing to record.
    let link = |ok: bool, status: u16, refused: Option<&str>| {
        a.record(crate::audit::AccountEvent {
            actor: who.person.clone().unwrap_or_else(|| "anonymous".into()),
            action: "identity.link".into(),
            method: "-".into(),
            target: who.person.clone(),
            ok,
            status,
            ip: who.ip,
            user_agent: who.user_agent.clone(),
            detail: serde_json::json!({ "method": crate::audit::clean_id(method), "refused": refused }),
        })
    };
    match &done {
        Err(_) if method.is_empty() => {}
        // A signed-in person's flow that failed was a link of theirs.
        Err(e) if who.person.is_some() => link(false, crate::audit::status_of(e), Some(e.redirect_code())),
        Err(e) => a.record_sign_in(None, method, Some(e), false, who),
        Ok(Finished::Linked { .. }) => link(true, 200, None),
        Ok(Finished::SignIn { user, new: Some(new), .. }) => a.record_new(user, new, method, who),
        Ok(Finished::SignIn { .. }) => {}
    }
    let web = a.config.web_url.clone();
    let redirect = |to: String, cookies: Vec<HeaderValue>| {
        let mut resp = (StatusCode::SEE_OTHER, [(header::LOCATION, HeaderValue::from_str(&to).unwrap_or(HeaderValue::from_static("/")))]).into_response();
        resp.headers_mut().append(header::SET_COOKIE, cookie(a, a.config.oauth_cookie(), "", 0));
        with_cookies(resp, cookies)
    };
    let fail = |back: &str, code: &str| {
        let sep = if back.contains('?') { '&' } else { '?' };
        redirect(format!("{web}{back}{sep}error={code}"), Vec::new())
    };
    match done {
        Err(e) => fail(&back, e.redirect_code()),
        Ok(Finished::Linked { return_to }) => redirect(format!("{web}{return_to}"), Vec::new()),
        Ok(Finished::SignIn { user, return_to, sid, .. }) => {
            let ua = user_agent(headers);
            let (u2, m2, w2) = (user.clone(), method.to_string(), who.clone());
            // An SSO sign-in's method is `sso:<connection>`: its session length applies.
            let connection = method.strip_prefix("sso:").unwrap_or_default().to_string();
            let how = m2.clone();
            match blocking(a, move |a| if sso { a.issue_sso(&user, &ua, &connection, sid.as_deref()) } else { a.issue(&user, &ua, &how) }).await {
                Err(e) => {
                    a.record_sign_in(Some(&u2), &m2, Some(&e), false, &w2);
                    fail("/signin", e.redirect_code())
                }
                Ok(issued) => {
                    a.record_sign_in(Some(&u2), &m2, None, issued.second_factor, &w2);
                    let ttl = if issued.second_factor { SECOND_FACTOR_TTL_MS } else { SESSION_TTL_MS };
                    let to = if issued.second_factor {
                        format!("{web}/signin?step=second_factor&return_to={}", url::form_urlencoded::byte_serialize(return_to.as_bytes()).collect::<String>())
                    } else {
                        format!("{web}{return_to}")
                    };
                    redirect(to, vec![cookie(a, a.config.session_cookie(), &issued.token, ttl)])
                }
            }
        }
    }
}

// ── SSO (docs/accounts.md, "SSO") ──

#[derive(Deserialize)]
struct SsoStart {
    #[serde(default)]
    email: String,
    #[serde(default)]
    intent: Option<String>,
    invite: Option<String>,
    return_to: Option<String>,
}

/// Start a sign-in through the SSO connection that answers for the email's domain (400 `no_sso`
/// if none does; the dashboard then offers the other ways), or (`intent: "link"`, a session) link
/// the signed-in person's account through the connection for their own email.
async fn sso_start(State(a): S, me: Me, b: Bytes) -> Result<Response> {
    let SsoStart { email, intent, invite, return_to } = body(&b)?;
    let link_user = match intent.as_deref().unwrap_or("signin") {
        "signin" => None,
        "link" => Some(signed_in(me)?.user),
        other => return Err(Error::BadRequest(format!("intent is signin or link, not {other}"), None)),
    };
    let started = a.sso_start(&email, invite.as_deref(), return_to.as_deref(), link_user).await?;
    let resp = Json(json!({ "url": started.url })).into_response();
    Ok(with_cookies(resp, [cookie(&a, a.config.oauth_cookie(), &started.browser, 10 * 60_000)]))
}

async fn sso_callback(State(a): S, me: Me, ip: Ip, Path(id): Path<String>, Query(q): Query<Callback>, headers: HeaderMap) -> Response {
    let who = Who::of(ip, &headers).person(me);
    let method = format!("sso:{}", crate::audit::clean_id(&id));
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return finish_redirect(&a, &headers, Err(Error::BadRequest("the SSO provider".into(), Some("provider"))), "/signin".into(), true, "", &who).await;
    };
    let browser = roll_auth::cookie(&headers, a.config.oauth_cookie()).map(str::to_string);
    let done = a.sso_callback(&id, &code, &state, browser).await;
    if let Err(e) = &done {
        tracing::info!(connection = %id, error = %e, "accounts: an SSO sign-in failed");
    }
    finish_redirect(&a, &headers, done, "/signin".into(), true, &method, &who).await
}

async fn list_sso(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.sso_connections()).await?).into_response())
}

async fn add_sso(State(a): S, me: Me, op: Op, b: Bytes) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let n: crate::sso::NewConnection = body(&b)?;
    let c = a.add_sso(n).await?;
    alert(&a, by, format!("the SSO connection {} was added, for {}", c.name, c.domains.join(", "))).await;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

async fn update_sso(State(a): S, me: Me, op: Op, Path(id): Path<String>, b: Bytes) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let ch: crate::sso::ConnectionChange = body(&b)?;
    // What changed, in words (never the secret itself).
    let mut what: Vec<String> = Vec::new();
    if let Some(e) = ch.enforce {
        what.push(format!("its sign-in enforced {}", on_off(e)));
    }
    if let Some(p) = ch.provision {
        what.push(format!("new people made at sign-in {}", on_off(p)));
    }
    if let Some(d) = &ch.domains {
        what.push(format!("its domains now {}", d.join(", ")));
    }
    if ch.client_id.is_some() || ch.client_secret.is_some() {
        what.push("its client credentials replaced".into());
    }
    if let Some(h) = ch.session_hours {
        what.push(if h == 0 { "its sessions' cap removed".into() } else { format!("its sessions last at most {h} hours") });
    }
    if let Some(n) = &ch.name {
        what.push(format!("renamed {n}"));
    }
    if let Some(g) = ch.groups {
        what.push(format!("its SCIM groups {}", on_off(g)));
    }
    let c = blocking(&a, move |a| a.update_sso(&id, ch)).await?;
    if !what.is_empty() {
        alert(&a, by, format!("the SSO connection {} changed: {}", c.name, what.join("; "))).await;
    }
    Ok(Json(c).into_response())
}

/// A SCIM token for the connection (replacing its old one): `{token, base_url}`, this once.
async fn scim_token(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let token = blocking(&a, {
        let id = id.clone();
        move |a| a.scim_token(&id)
    })
    .await?;
    let name = blocking(&a, move |a| Ok(a.sso_name(&id))).await?;
    alert(&a, by, format!("a new SCIM token was made for {name} (its provider can now create and turn off people; the old token stopped)")).await;
    let base = format!("{}/v1/accounts/scim/v2", a.config.api_url);
    Ok(with_cookies((StatusCode::CREATED, Json(json!({ "token": token, "base_url": base }))).into_response(), None))
}

async fn remove_scim_token(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let name = blocking(&a, move |a| a.remove_scim_token(&id).map(|()| a.sso_name(&id))).await?;
    alert(&a, by, format!("the SCIM token for {name} was removed")).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct LogoutForm {
    logout_token: String,
}

/// OIDC Back-Channel Logout 1.0 (docs/accounts.md, "SSO"): the provider posts a signed logout
/// token; the sessions it names end. No session or token: the token is the credential.
async fn backchannel_logout(State(a): S, Path(id): Path<String>, axum::extract::Form(f): axum::extract::Form<LogoutForm>) -> Response {
    let no_store = |mut r: Response| {
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    };
    match a.backchannel_logout(&id, &f.logout_token).await {
        Ok(_) => no_store(StatusCode::OK.into_response()),
        Err(e) => no_store(e.into_response()),
    }
}

/// Check the connection's domains' TXT records now; the ones that have it are verified.
async fn verify_sso(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(a.verify_sso(&id).await?).into_response())
}

async fn remove_sso(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let name = blocking(&a, move |a| {
        let name = a.sso_name(&id);
        a.remove_sso(&id).map(|()| name)
    })
    .await?;
    alert(&a, by, format!("the SSO connection {name} was removed (its people sign in other ways now)")).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

async fn second_step(a: Arc<Accounts>, owed: Owed, ip: Ip, headers: HeaderMap, b: Bytes, recovery: bool) -> Result<Response> {
    let Some(Extension(owed)) = owed else { return Err(Error::Unauthorized("no sign-in is waiting for a second factor".into())) };
    let Code { code } = body(&b)?;
    let ua = user_agent(&headers);
    let who = Who::of(ip, &headers);
    let method = if recovery { "recovery_code" } else { "totp" };
    blocking(&a, move |a| {
        let done = (|| {
            let ok = if recovery { a.check_recovery(&owed.user, &code)? } else { a.check_totp(&owed.user, &code, true)? };
            if !ok {
                return Err(Error::bad_code());
            }
            a.complete(&owed, &ua)
        })();
        // The person is known (they owe a second factor): a wrong code is theirs.
        match done {
            Ok(issued) => {
                a.record_sign_in(Some(&owed.user), method, None, false, &who);
                signed_in_response(a, &owed.user, issued)
            }
            Err(e) => {
                a.record_sign_in(Some(&owed.user), method, Some(&e), false, &who);
                Err(e)
            }
        }
    })
    .await
}

async fn signin_totp(State(a): S, owed: Owed, ip: Ip, headers: HeaderMap, b: Bytes) -> Result<Response> {
    second_step(a, owed, ip, headers, b, false).await
}

async fn signin_recovery(State(a): S, owed: Owed, ip: Ip, headers: HeaderMap, b: Bytes) -> Result<Response> {
    second_step(a, owed, ip, headers, b, true).await
}

// ── passkeys (docs/accounts.md, "Passkeys") ──

/// A passkey sign-in's challenge: for the person owing a second factor, their passkeys; for
/// anyone else, any passkey the browser holds for roll (throttled by address: no account yet).
async fn signin_passkey_options(State(a): S, owed: Owed, ip: Ip) -> Result<Json<Value>> {
    let owed = owed.map(|Extension(o)| o.user);
    let from = ip.map(|Extension(roll_auth::ClientIp(i))| i.to_string()).unwrap_or_default();
    Ok(Json(
        blocking(&a, move |a| {
            if owed.is_none() && !a.attempt(&format!("passkey:{from}"), 60, 10 * 60 * 1000)? {
                return Err(Error::Unavailable("too many passkey sign-ins from this address: try again in a few minutes".into(), "too_many_requests"));
            }
            a.passkey_signin_options(owed.as_deref())
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct PasskeyAnswer {
    /// The challenge's id, from the options.
    id: String,
    /// The browser's `PublicKeyCredential`, as JSON (`id`, `response`).
    credential: Value,
    #[serde(default)]
    name: Option<String>,
}

/// A passkey sign-in: the second factor of a sign-in owing one, or, on its own, a whole sign-in
/// (the authenticator verified the person).
async fn signin_passkey(State(a): S, owed: Owed, ip: Ip, headers: HeaderMap, b: Bytes) -> Result<Response> {
    let PasskeyAnswer { id, credential, .. } = body(&b)?;
    let ua = user_agent(&headers);
    let who = Who::of(ip, &headers);
    let owed = owed.map(|Extension(o)| o);
    blocking(&a, move |a| {
        let checked = a.passkey_check(&id, &credential, owed.as_ref().map(|o| o.user.as_str()));
        let user = checked.as_ref().ok().cloned().or_else(|| owed.as_ref().map(|o| o.user.clone()));
        let done = checked.and_then(|user| match &owed {
            // The challenge was this owing person's, and the passkey theirs (both checked).
            Some(o) => a.complete(o, &ua).and_then(|issued| a.mark_passkey(&issued.id).map(|()| issued)),
            None => a.issue_passkey(&user, &ua),
        });
        match (done, user) {
            (Ok(issued), Some(user)) => {
                a.record_sign_in(Some(&user), "passkey", None, false, &who);
                signed_in_response(a, &user, issued)
            }
            (Err(e), user) => {
                // A passkey names its account: a refusal is that person's (by nobody known, unless
                // they were already signed in up to their second factor).
                match (&owed, user) {
                    (Some(o), _) => a.record_sign_in(Some(&o.user), "passkey", Some(&e), false, &who),
                    (None, u) => a.record_code_refused(u.or_else(|| a.passkey_owner(&credential)).as_deref(), "passkey", &e, &who),
                }
                Err(e)
            }
            (Ok(_), None) => Err(Error::Internal("a passkey sign-in without a person".into())),
        }
    })
    .await
}

// ── second-factor resets (docs/accounts.md, "Recovery") ──

#[derive(Deserialize)]
struct ResetAsk {
    reason: String,
}

/// Someone signed in up to their second factor, who lost it, asks an admin to reset it.
async fn request_reset(State(a): S, owed: Owed, ip: Ip, headers: HeaderMap, b: Bytes) -> Result<(StatusCode, Json<Value>)> {
    let Some(Extension(owed)) = owed else { return Err(Error::Unauthorized("sign in with your first factor first (an email code, GitHub, Google)".into())) };
    let ResetAsk { reason } = body(&b)?;
    let who = Who::of(ip, &headers);
    let a2 = a.clone();
    let req = blocking(&a, move |_| a2.request_reset(&owed.user, &reason, &who)).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "id": req.id, "state": req.state, "created_at": req.created_at }))))
}

/// Hardware keys only (docs/accounts.md, "Passkeys"): which authenticator models count.
async fn passkey_policy(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.passkey_policy()).await?).into_response())
}

async fn set_passkey_policy(State(a): S, me: Me, op: Op, b: Bytes) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let p: crate::attestation::Policy = body(&b)?;
    let p = blocking(&a, move |a| a.set_passkey_policy(p)).await?;
    let models: Vec<&str> = p.models.iter().map(|m| m.name.as_str()).collect();
    alert(&a, by, format!("hardware keys only is {} ({})", on_off(p.require), if models.is_empty() { "no models listed".into() } else { models.join(", ") })).await;
    Ok(Json(p).into_response())
}

/// Account policy (docs/accounts.md): a second factor for everyone.
async fn account_policy(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.account_policy()).await?).into_response())
}

async fn set_account_policy(State(a): S, me: Me, op: Op, b: Bytes) -> Result<Response> {
    let actor = admin_sudo(&a, &me, &op).await?;
    let p: crate::policy::AccountPolicy = body(&b)?;
    let by = me.map(|Extension(u)| (u.user, u.session));
    let p = blocking(&a, move |a| a.set_account_policy_by(p, by.as_ref().map(|(u, s)| (u.as_str(), s.as_str())))).await?;
    let idle = p.idle_minutes.map_or_else(|| "a week".to_string(), |m| format!("{m} minutes"));
    let passkeys = match p.phishing_resistant {
        Some(crate::policy::Whom::Admins) => "admins",
        Some(crate::policy::Whom::Everyone) => "everyone",
        None => "nobody",
    };
    let longest = p.max_session_hours.map_or_else(|| "30 days".to_string(), |h| format!("{h} hours"));
    alert(&a, actor, format!("the sign-in policy changed: a second factor for everyone {}, sessions end after {idle} unused and {longest} after sign-in, passkeys required for {passkeys}", on_off(p.require_second_factor))).await;
    Ok(Json(p).into_response())
}

/// Allowed networks (docs/accounts.md): where people's sessions may be used from.
async fn network_policy(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.network_policy()).await?).into_response())
}

async fn set_network_policy(State(a): S, me: Me, op: Op, ip: Ip, b: Bytes) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let p: crate::networks::NetworkPolicy = body(&b)?;
    // An admin's own address must stay on it; the operator's token has no session to lock out.
    let from = me.as_ref().and(ip.map(|Extension(roll_auth::ClientIp(i))| i));
    let p = blocking(&a, move |a| a.set_network_policy(p, from)).await?;
    alert(&a, by, format!("allowed networks are {} ({})", on_off(p.on), p.networks.join(", "))).await;
    Ok(Json(p).into_response())
}

async fn list_resets(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.reset_requests()).await?).into_response())
}

async fn decide(a: Arc<Accounts>, me: Me, op: Op, ip: Ip, headers: HeaderMap, id: String, approve: bool) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let who = Who::of(ip, &headers);
    let a2 = a.clone();
    Ok(Json(blocking(&a, move |_| a2.decide_reset(&id, approve, &by, &who)).await?).into_response())
}

async fn approve_reset(State(a): S, me: Me, op: Op, ip: Ip, headers: HeaderMap, Path(id): Path<String>) -> Result<Response> {
    decide(a, me, op, ip, headers, id, true).await
}

async fn deny_reset(State(a): S, me: Me, op: Op, ip: Ip, headers: HeaderMap, Path(id): Path<String>) -> Result<Response> {
    decide(a, me, op, ip, headers, id, false).await
}

/// Your data (docs/accounts.md, "Your data"): one JSON file, a download, behind sudo mode.
async fn export_me(State(a): S, me: Me, ip: Ip, headers: HeaderMap) -> Result<Response> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    let who = Who::of(ip, &headers);
    let (user, session) = (u.user.clone(), u.session.clone());
    let data = blocking(&a, move |a| a.export(&user, &session)).await?;
    a.record(crate::audit::AccountEvent { actor: u.user.clone(), action: "account.export".into(), method: "-".into(), target: Some(u.user.clone()), ok: true, status: 200, ip: who.ip, user_agent: who.user_agent, detail: json!({}) });
    let name = format!("roll-account-{}.json", u.handle.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>());
    let mut resp = Json(data).into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

// ── sudo mode (docs/accounts.md, "Sudo mode") ──

/// How long this session's proof lasts, and how its person can confirm it's them.
async fn reauth_info(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    Ok(Json(blocking(&a, move |a| Ok(json!({ "fresh_until": a.fresh_until(&u.session)?, "methods": a.reauth_methods(&u.user)? }))).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reauth {
    /// A TOTP or recovery code; or, with `challenge`, the code an email carried.
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    challenge: Option<String>,
    /// A passkey: its challenge's id and the browser's credential.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    credential: Option<Value>,
}

/// Confirm it's you: the session is fresh for another two hours.
async fn reauth(State(a): S, me: Me, ip: Ip, headers: HeaderMap, b: Bytes) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    let r: Reauth = body(&b)?;
    let who = Who::of(ip, &headers);
    Ok(Json(
        blocking(&a, move |a| {
            let (method, proved) = match r {
                Reauth { id: Some(id), credential: Some(c), .. } => ("passkey", a.passkey_check_for("reauth", &id, &c, Some(&u.user)).map(|_| ())),
                Reauth { challenge: Some(ch), code: Some(code), .. } => ("email", match a.email_verify(&ch, &code) {
                    Ok((user, _)) if user == u.user => Ok(()),
                    Ok(_) => Err(Error::bad_code()),
                    Err(e) => Err(e),
                }),
                Reauth { code: Some(code), .. } => ("totp", match a.second_factor(&u.user, &code) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(Error::bad_code()),
                    Err(e) => Err(e),
                }),
                _ => return Err(Error::BadRequest("a code, an email's challenge and code, or a passkey".into(), None)),
            };
            let done = proved.and_then(|()| a.mark_verified(&u.user, &u.session)).and_then(|until| {
                // A passkey used now makes the session phishing-resistant (docs/accounts.md).
                if method == "passkey" {
                    a.mark_passkey(&u.session)?;
                }
                Ok(until)
            });
            a.record_reauth(&u.user, method, done.as_ref().err(), &who);
            Ok(json!({ "fresh_until": done? }))
        })
        .await?,
    ))
}

/// An email code to confirm it's you, to your own address: `{challenge}`.
async fn reauth_email(State(a): S, me: Me) -> Result<(StatusCode, Json<Value>)> {
    let u = signed_in(me)?;
    if !a.mail_on() {
        return Err(Error::Unavailable("email isn't configured on this server".into(), "email_off"));
    }
    let challenge = crate::new_id("chl");
    let (a2, id) = (a.clone(), challenge.clone());
    let mail = blocking(&a2, move |a| {
        let email = a.user(&u.user)?.email;
        a.email_start(&id, &email, None)
    })
    .await?;
    if let Some(mail) = mail {
        a.send_later(mail, "confirmation code");
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "challenge": challenge }))))
}

/// A passkey challenge to confirm it's you (your own passkeys only).
async fn reauth_passkey_options(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    Ok(Json(blocking(&a, move |a| a.passkey_assert_options(Some(&u.user), "reauth")).await?))
}

async fn my_passkeys(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    Ok(Json(json!({ "passkeys": blocking(&a, move |a| a.passkeys(&u.user)).await? })))
}

async fn passkey_options(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    Ok(Json(blocking(&a, move |a| a.passkey_register_options(&u.user)).await?))
}

async fn add_passkey(State(a): S, me: Me, b: Bytes) -> Result<(StatusCode, Json<Value>)> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    let PasskeyAnswer { id, credential, name } = body(&b)?;
    let made = blocking(&a, move |a| a.passkey_add(&u.user, &id, name.as_deref().unwrap_or("Passkey"), &credential)).await?;
    Ok((StatusCode::CREATED, Json(json!(made))))
}

async fn rename_passkey(State(a): S, me: Me, Path(id): Path<String>, b: Bytes) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    let Name { name } = body(&b)?;
    Ok(Json(json!(blocking(&a, move |a| a.passkey_rename(&u.user, &id, &name)).await?)))
}

async fn remove_passkey(State(a): S, me: Me, Path(id): Path<String>) -> Result<StatusCode> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    blocking(&a, move |a| a.passkey_remove(&u.user, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

async fn peek_invite(State(a): S, b: Bytes) -> Result<Json<Value>> {
    let Token { token } = body(&b)?;
    let (email, expires_at) = blocking(&a, move |a| a.peek_invite(&token)).await?;
    Ok(Json(json!({ "email": email, "expires_at": expires_at })))
}

// ── your account ──

async fn me(State(a): S, me: Me) -> Result<Response> {
    let u = signed_in(me)?;
    Ok(Json(blocking(&a, move |a| a.user(&u.user)).await?).into_response())
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

async fn update_me(State(a): S, me: Me, b: Bytes) -> Result<Response> {
    let u = signed_in(me)?;
    let Name { name } = body(&b)?;
    Ok(Json(blocking(&a, move |a| a.rename(&u.user, &name)).await?).into_response())
}

#[derive(Deserialize)]
struct Confirm {
    confirm: String,
}

/// Delete your own account (docs/accounts.md, "Deleting your account"): signed out here too.
async fn delete_me(State(a): S, me: Me, b: Bytes) -> Result<Response> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    let Confirm { confirm } = body(&b)?;
    let mail = blocking(&a, move |a| a.delete_self(&u.user, &confirm)).await?;
    if a.mail_on() {
        a.send_later(mail, "account deleted");
    }
    Ok(with_cookies(StatusCode::NO_CONTENT.into_response(), [cookie(&a, a.config.session_cookie(), "", 0)]))
}

#[derive(Deserialize)]
struct NewEmail {
    email: String,
}

/// Move your account to another address: a code goes there (docs/accounts.md, "Your address").
async fn change_email(State(a): S, me: Me, b: Bytes) -> Result<(StatusCode, Json<Value>)> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    if !a.mail_on() {
        return Err(Error::Unavailable("email isn't configured on this server".into(), "email_off"));
    }
    let NewEmail { email } = body(&b)?;
    let challenge = crate::new_id("chl");
    let id = challenge.clone();
    if let Some(mail) = blocking(&a, move |a| a.email_change_start(&u.user, &id, &email)).await? {
        a.send_later(mail, "address change code");
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "challenge": challenge }))))
}

/// The code from the new address: the account moves, and the old address is told.
async fn confirm_email(State(a): S, me: Me, b: Bytes) -> Result<Json<crate::User>> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    let Verify { challenge, code } = body(&b)?;
    let (user, mail) = blocking(&a, move |a| a.email_change_confirm(&u.user, &challenge, &code)).await?;
    a.send_later(mail, "address changed");
    Ok(Json(user))
}

#[derive(Deserialize)]
struct ActivityQuery {
    before: Option<i64>,
    limit: Option<usize>,
}

/// A person's own security log: their sign-ins (and refusals) and every change to their account,
/// newest first (`before`: older pages).
async fn my_activity(State(a): S, me: Me, Query(q): Query<ActivityQuery>) -> Result<Response> {
    let u = signed_in(me)?;
    let events = blocking(&a, move |a| Ok(a.activity(&u.user, q.before, q.limit.unwrap_or(50)))).await?;
    Ok(Json(json!({ "events": events })).into_response())
}

async fn my_sessions(State(a): S, me: Me) -> Result<Response> {
    let u = signed_in(me)?;
    Ok(Json(blocking(&a, move |a| a.sessions_of(&u.user, &u.session)).await?).into_response())
}

/// End every session of yours but this one (Google's "sign out of all other sessions"): what to
/// do first when Recent activity shows a sign-in you don't recognise.
async fn end_other_sessions(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    let ended = blocking(&a, move |a| a.end_other_sessions(&u.user, &u.session)).await?;
    Ok(Json(json!({ "ended": ended })))
}

async fn revoke_my_session(State(a): S, me: Me, Path(id): Path<String>) -> Result<StatusCode> {
    let u = signed_in(me)?;
    blocking(&a, move |a| a.revoke_session(&u.user, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unlink(State(a): S, me: Me, Path(p): Path<String>) -> Result<StatusCode> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    // GitHub, Google, or an SSO connection's link (`sso:<id>`).
    let p = match p.strip_prefix("sso:") {
        Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => p.clone(),
        _ => provider(&p)?.as_str().to_string(),
    };
    blocking(&a, move |a| a.unlink(&u.user, &p)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn totp_enrol(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    sudo(&a, &u).await?;
    let e = blocking(&a, move |a| a.totp_enrol(&u.user)).await?;
    Ok(Json(json!({ "secret": e.secret, "otpauth_url": e.otpauth_url, "qr_png": e.qr_png })))
}

async fn totp_confirm(State(a): S, me: Me, b: Bytes) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    let Code { code } = body(&b)?;
    let codes = blocking(&a, move |a| a.totp_confirm(&u.user, &code)).await?;
    Ok(Json(json!({ "recovery_codes": codes })))
}

async fn totp_disable(State(a): S, me: Me, b: Bytes) -> Result<StatusCode> {
    let u = signed_in(me)?;
    let Code { code } = body(&b)?;
    blocking(&a, move |a| a.totp_disable(&u.user, &code)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn recovery_codes(State(a): S, me: Me, b: Bytes) -> Result<Json<Value>> {
    let u = signed_in(me)?;
    let Code { code } = body(&b)?;
    let codes = blocking(&a, move |a| a.regenerate_recovery_codes(&u.user, &code)).await?;
    Ok(Json(json!({ "recovery_codes": codes })))
}

// ── admin ──

#[derive(Deserialize)]
struct NewInvite {
    email: String,
    #[serde(default)]
    admin: bool,
    handle: Option<String>,
}

async fn create_invite(State(a): S, me: Me, op: Op, b: Bytes) -> Result<Response> {
    let by = admin(&a, &me, &op)?;
    let NewInvite { email, admin, handle } = body(&b)?;
    // Making an admin is sudo; inviting someone else isn't.
    if let (true, Some(Extension(u))) = (admin, &me) {
        sudo(&a, u).await?;
    }
    let actor = by.clone();
    let (invite, token, url) = blocking(&a, move |a| a.invite(&by, &email, admin, handle.as_deref())).await?;
    if admin {
        alert(&a, actor, format!("an invite that makes an admin was made for {}", invite.email)).await;
    }
    invite_sent(&a, invite, token, url, StatusCode::CREATED)
}

/// An invite's link, mailed to it and answered once (with its token): a new invite or a resent one.
fn invite_sent(a: &Arc<Accounts>, invite: crate::Invite, token: String, url: String, status: StatusCode) -> Result<Response> {
    if a.mail_on() {
        let text = format!("You're invited to roll.\n\nSign in here within 7 days: {url}\n\nThe link works once, for {}.\n", invite.email);
        a.send_later(crate::Mail { to: invite.email.clone(), subject: "You're invited to roll".into(), text }, "invite");
    }
    let mut v = serde_json::to_value(&invite).map_err(|e| Error::Internal(e.to_string()))?;
    v["token"] = token.into();
    v["url"] = url.into();
    Ok(with_cookies((status, Json(v)).into_response(), None))
}

/// Send an unused invite again (Okta's and GitHub's "resend"): a fresh link for another 7 days,
/// mailed again; the old link stops at once. An invite that makes an admin needs sudo mode, as
/// making it did.
async fn resend_invite(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<Response> {
    admin(&a, &me, &op)?;
    let makes_admin = blocking(&a, {
        let id = id.clone();
        move |a| Ok(a.invites()?.into_iter().find(|i| i.id == id).is_some_and(|i| i.admin))
    })
    .await?;
    if let (true, Some(Extension(u))) = (makes_admin, &me) {
        sudo(&a, u).await?;
    }
    let (invite, token, url) = blocking(&a, move |a| a.resend_invite(&id)).await?;
    invite_sent(&a, invite, token, url, StatusCode::OK)
}

async fn list_invites(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.invites()).await?).into_response())
}

async fn revoke_invite(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<StatusCode> {
    admin(&a, &me, &op)?;
    blocking(&a, move |a| a.revoke_invite(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_users(State(a): S, me: Me, op: Op) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, |a| a.users()).await?).into_response())
}

#[derive(Deserialize)]
struct UserPatch {
    admin: Option<bool>,
    disabled: Option<bool>,
}

async fn update_user(State(a): S, me: Me, op: Op, Path(id): Path<String>, b: Bytes) -> Result<Response> {
    let by = admin_sudo(&a, &me, &op).await?;
    let UserPatch { admin, disabled } = body(&b)?;
    let was_admin = blocking(&a, {
        let id = id.clone();
        move |a| Ok(a.user(&id)?.admin)
    })
    .await?;
    let target = blocking(&a, {
        let id = id.clone();
        move |a| a.user(&id)
    })
    .await?;
    // The change is committed before a failure to cut off its roll code owner shows: the alert
    // says what was done either way.
    let done = blocking(&a, move |a| a.update_user(&id, admin, disabled)).await;
    let who = format!("{} ({})", target.handle, target.email);
    let mut what: Vec<String> = Vec::new();
    match admin {
        Some(true) if !was_admin => what.push(format!("{who} was made an admin")),
        Some(false) if was_admin => what.push(format!("{who} is no longer an admin")),
        _ => {}
    }
    if disabled == Some(true) && was_admin {
        what.push(format!("the admin {who}'s account was turned off"));
    }
    if !what.is_empty() {
        alert(&a, by, what.join("; ")).await;
    }
    Ok(Json(done?).into_response())
}

/// A person's security log, as an admin reads it from People (Okta's per-user System Log): the
/// same events the person sees of themselves.
async fn user_activity(State(a): S, me: Me, op: Op, Path(id): Path<String>, Query(q): Query<ActivityQuery>) -> Result<Response> {
    admin(&a, &me, &op)?;
    let events = blocking(&a, move |a| {
        a.user(&id)?;
        Ok(a.activity(&id, q.before, q.limit.unwrap_or(50)))
    })
    .await?;
    Ok(Json(json!({ "events": events })).into_response())
}

/// A person's signed-in sessions, for an admin (none is "current": it's not the admin's own).
async fn user_sessions(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<Response> {
    admin(&a, &me, &op)?;
    Ok(Json(blocking(&a, move |a| {
        a.user(&id)?;
        a.sessions_of(&id, "")
    })
    .await?)
    .into_response())
}

/// End one of a person's sessions (a lost laptop), leaving the others.
async fn revoke_user_session(State(a): S, me: Me, op: Op, Path((id, session)): Path<(String, String)>) -> Result<StatusCode> {
    admin(&a, &me, &op)?;
    blocking(&a, move |a| a.revoke_session(&id, &session)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_user_sessions(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<StatusCode> {
    admin(&a, &me, &op)?;
    blocking(&a, move |a| a.revoke_sessions(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
