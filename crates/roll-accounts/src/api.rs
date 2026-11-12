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
        .route("/v1/accounts/me", get(me).patch(update_me))
        .route("/v1/accounts/me/sessions", get(my_sessions))
        .route("/v1/accounts/me/sessions/{id}", delete(revoke_my_session))
        .route("/v1/accounts/me/identities/{provider}", delete(unlink))
        .route("/v1/accounts/me/totp", post(totp_enrol).delete(totp_disable))
        .route("/v1/accounts/me/totp/confirm", post(totp_confirm))
        .route("/v1/accounts/me/recovery-codes", post(recovery_codes))
        .route("/v1/accounts/invites", get(list_invites).post(create_invite))
        .route("/v1/accounts/invites/peek", post(peek_invite))
        .route("/v1/accounts/invites/{id}", delete(revoke_invite))
        .route("/v1/accounts/users", get(list_users))
        .route("/v1/accounts/users/{id}", axum::routing::patch(update_user))
        .route("/v1/accounts/users/{id}/sessions", delete(revoke_user_sessions))
        .route_layer(axum::middleware::from_fn(json_only))
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
    let form = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
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

/// The body as JSON, a 400 in the API's shape if it isn't.
fn body<T: DeserializeOwned>(b: &Bytes) -> Result<T> {
    let b: &[u8] = if b.iter().all(u8::is_ascii_whitespace) { b"{}" } else { b };
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}"), None))
}

fn signed_in(me: Me) -> Result<SignedIn> {
    me.map(|Extension(u)| u).ok_or_else(Error::signed_out)
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
        return Ok(json!({ "status": "mfa_required", "methods": ["totp", "recovery"], "csrf": csrf }));
    }
    Ok(json!({ "status": "signed_in", "user": a.user(user)?, "session": a.session_info(session)?, "csrf": csrf }))
}

/// A new session's answer, with its cookie.
fn signed_in_response(a: &Accounts, user: &str, issued: Issued) -> Result<Response> {
    let body = session_body(a, &issued.token, user, &issued.id, issued.second_factor)?;
    let ttl = if issued.second_factor { SECOND_FACTOR_TTL_MS } else { SESSION_TTL_MS };
    Ok(with_cookies(Json(body).into_response(), [cookie(a, a.config.session_cookie(), &issued.token, ttl)]))
}

// ── public ──

async fn info(State(a): S) -> Json<Value> {
    Json(json!({ "providers": { "email": a.mail_on(), "github": a.config.github.is_some(), "google": a.config.google.is_some() } }))
}

async fn session(State(a): S, me: Me, owed: Owed, headers: HeaderMap) -> Result<Response> {
    let token = roll_auth::cookie(&headers, a.config.session_cookie()).map(str::to_string);
    let body = match (me, owed, &token) {
        (Some(Extension(u)), _, Some(t)) => {
            let t = t.clone();
            blocking(&a, move |a| session_body(a, &t, &u.user, &u.session, false)).await?
        }
        (None, Some(_), Some(t)) => json!({ "status": "mfa_required", "methods": ["totp", "recovery"], "csrf": roll_auth::csrf_token(t) }),
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

async fn email_verify(State(a): S, headers: HeaderMap, b: Bytes) -> Result<Response> {
    let Verify { challenge, code } = body(&b)?;
    let ua = user_agent(&headers);
    blocking(&a, move |a| {
        let user = a.email_verify(&challenge, &code)?;
        let issued = a.issue(&user, &ua)?;
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

async fn oauth_callback(State(a): S, Path(p): Path<String>, Query(q): Query<Callback>, headers: HeaderMap) -> Response {
    let web = a.config.web_url.clone();
    let redirect = |to: String, cookies: Vec<HeaderValue>| {
        let mut resp = (StatusCode::SEE_OTHER, [(header::LOCATION, HeaderValue::from_str(&to).unwrap_or(HeaderValue::from_static("/")))]).into_response();
        resp.headers_mut().append(header::SET_COOKIE, cookie(&a, a.config.oauth_cookie(), "", 0));
        with_cookies(resp, cookies)
    };
    let fail = |back: &str, code: &str| {
        let sep = if back.contains('?') { '&' } else { '?' };
        redirect(format!("{web}{back}{sep}error={code}"), Vec::new())
    };
    let Some(p) = Provider::parse(&p) else { return fail("/signin", "provider") };
    let (Some(code), Some(state)) = (q.code, q.state) else {
        // The person said no at the provider, or the provider failed.
        let _ = q.error;
        return fail("/signin", "provider");
    };
    let browser = roll_auth::cookie(&headers, a.config.oauth_cookie()).map(str::to_string);
    let (done, back) = a.oauth_callback(p, &code, &state, browser).await;
    match done {
        Err(e) => fail(&back, e.redirect_code()),
        Ok(Finished::Linked { return_to }) => redirect(format!("{web}{return_to}"), Vec::new()),
        Ok(Finished::SignIn { user, return_to }) => {
            let ua = user_agent(&headers);
            match blocking(&a, move |a| a.issue(&user, &ua)).await {
                Err(e) => fail("/signin", e.redirect_code()),
                Ok(issued) => {
                    let ttl = if issued.second_factor { SECOND_FACTOR_TTL_MS } else { SESSION_TTL_MS };
                    let to = if issued.second_factor {
                        format!("{web}/signin?step=second_factor&return_to={}", url::form_urlencoded::byte_serialize(return_to.as_bytes()).collect::<String>())
                    } else {
                        format!("{web}{return_to}")
                    };
                    redirect(to, vec![cookie(&a, a.config.session_cookie(), &issued.token, ttl)])
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

async fn second_step(a: Arc<Accounts>, owed: Owed, headers: HeaderMap, b: Bytes, recovery: bool) -> Result<Response> {
    let Some(Extension(owed)) = owed else { return Err(Error::Unauthorized("no sign-in is waiting for a second factor".into())) };
    let Code { code } = body(&b)?;
    let ua = user_agent(&headers);
    blocking(&a, move |a| {
        let ok = if recovery { a.check_recovery(&owed.user, &code)? } else { a.check_totp(&owed.user, &code, true)? };
        if !ok {
            return Err(Error::bad_code());
        }
        let issued = a.complete(&owed, &ua)?;
        signed_in_response(a, &owed.user, issued)
    })
    .await
}

async fn signin_totp(State(a): S, owed: Owed, headers: HeaderMap, b: Bytes) -> Result<Response> {
    second_step(a, owed, headers, b, false).await
}

async fn signin_recovery(State(a): S, owed: Owed, headers: HeaderMap, b: Bytes) -> Result<Response> {
    second_step(a, owed, headers, b, true).await
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

async fn my_sessions(State(a): S, me: Me) -> Result<Response> {
    let u = signed_in(me)?;
    Ok(Json(blocking(&a, move |a| a.sessions_of(&u.user, &u.session)).await?).into_response())
}

async fn revoke_my_session(State(a): S, me: Me, Path(id): Path<String>) -> Result<StatusCode> {
    let u = signed_in(me)?;
    blocking(&a, move |a| a.revoke_session(&u.user, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unlink(State(a): S, me: Me, Path(p): Path<String>) -> Result<StatusCode> {
    let u = signed_in(me)?;
    let p = provider(&p)?;
    blocking(&a, move |a| a.unlink(&u.user, p.as_str())).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn totp_enrol(State(a): S, me: Me) -> Result<Json<Value>> {
    let u = signed_in(me)?;
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
    let (invite, token, url) = blocking(&a, move |a| a.invite(&by, &email, admin, handle.as_deref())).await?;
    if a.mail_on() {
        let text = format!("You're invited to roll.\n\nSign in here within 7 days: {url}\n\nThe link works once, for {}.\n", invite.email);
        a.send_later(crate::Mail { to: invite.email.clone(), subject: "You're invited to roll".into(), text }, "invite");
    }
    let mut v = serde_json::to_value(&invite).map_err(|e| Error::Internal(e.to_string()))?;
    v["token"] = token.into();
    v["url"] = url.into();
    Ok(with_cookies((StatusCode::CREATED, Json(v)).into_response(), None))
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
    admin(&a, &me, &op)?;
    let UserPatch { admin, disabled } = body(&b)?;
    Ok(Json(blocking(&a, move |a| a.update_user(&id, admin, disabled)).await?).into_response())
}

async fn revoke_user_sessions(State(a): S, me: Me, op: Op, Path(id): Path<String>) -> Result<StatusCode> {
    admin(&a, &me, &op)?;
    blocking(&a, move |a| a.revoke_sessions(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
