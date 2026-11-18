//! People's accounts in the audit log (docs/accounts.md, "Audit"): who signed in and how (and
//! who failed to), and every change to accounts, invites, SSO connections and SCIM, as Okta's
//! System Log and GitHub's audit log keep them. rolld writes them to roll-access's audit log (its
//! page, filters and SIEM sinks); roll-accounts only says what happened.
//!
//! - Every write to an accounts route is recorded once it's answered, in a task of its own (a
//!   client that drops its connection doesn't drop the event): its method and route, the id it
//!   names, who made it (a person's id, `operator`, `anonymous`), its status, the client's address
//!   and user agent, which of a short list of non-secret fields it set (`admin`, `disabled`,
//!   `enforce`…: never a code, token, secret or password), and the id it made.
//! - Sign-ins (email code, GitHub, Google, SSO, a second factor), the accounts and links they made,
//!   and SCIM's changes are recorded by their handlers, which know the person and the method.
//! - An anonymous request refused before it touched anything (401, 403, 404, 405, 415) on a route
//!   that isn't a sign-in isn't recorded: nobody can fill the log by knocking.

use crate::{Accounts, Error};
use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use roll_auth::{ClientIp, Operator, SignedIn};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::net::IpAddr;
use std::sync::Arc;

/// Bodies read for their fields (an accounts request or answer is a few hundred bytes).
const MAX_BODY: usize = 256 << 10;
/// The fields of a request an event may carry: none of them is ever a secret.
const FIELDS: &[&str] = &["admin", "disabled", "email", "handle", "enforce", "provision", "domains", "intent", "issuer", "client_id"];

/// Where account events go (rolld: roll-access's audit log), and how a request's client address
/// is read (its trusted proxies).
pub trait Audit: Send + Sync {
    fn record(&self, e: AccountEvent);
    fn client_ip(&self, req: &Request) -> Option<IpAddr>;
    /// The events about `user` (their sign-ins and changes to their account), newest first,
    /// before `before` (Unix ms), at most `limit`: a person's own security log.
    fn activity(&self, user: &str, before: Option<i64>, limit: usize) -> Vec<Activity>;
}

/// One line of a person's security log.
#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub at: i64,
    /// `sign_in`, `user.create`, `identity.link`, `scim.update`, or `METHOD /route`.
    pub action: String,
    /// Who did it: the person, `operator`, another person's id, `scim:<connection>`.
    pub actor: String,
    pub ok: bool,
    pub status: u16,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub detail: Value,
}

/// One thing that happened to accounts.
#[derive(Clone, Debug, Serialize)]
pub struct AccountEvent {
    /// A person's id, `operator`, `anonymous`, or `scim:<connection>`.
    pub actor: String,
    /// `sign_in`, `user.create`, `identity.link`, `scim.update`, or the request's `METHOD /route`.
    pub action: String,
    pub method: String,
    /// The person or object it's about.
    pub target: Option<String>,
    pub ok: bool,
    pub status: u16,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    /// What else matters (a sign-in's method, why it was refused, the fields a change set):
    /// never a secret.
    pub detail: Value,
}

/// Where a request came from, and who made it, for its events.
#[derive(Clone, Debug, Default)]
pub struct Who {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    /// The person signed in on the browser (a link's owner), if any.
    pub person: Option<String>,
}

impl Who {
    pub(crate) fn of(ip: Option<axum::Extension<ClientIp>>, headers: &axum::http::HeaderMap) -> Who {
        let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().filter(|c| !c.is_control()).take(120).collect::<String>());
        Who { ip: ip.map(|axum::Extension(ClientIp(ip))| ip), user_agent: ua, person: None }
    }

    pub(crate) fn person(mut self, me: Option<axum::Extension<SignedIn>>) -> Who {
        self.person = me.map(|axum::Extension(u)| u.user);
        self
    }
}

/// An id fit for the log: what roll's ids look like (letters, digits, `_`, `-`, `:`, `.`, `@`,
/// at most 80), else `invalid`.
pub(crate) fn clean_id(s: &str) -> String {
    let ok = !s.is_empty() && s.len() <= 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.' | b'@'));
    if ok { s.to_string() } else { "invalid".into() }
}

/// The status an error answers with (as the API's errors do).
pub(crate) fn status_of(e: &Error) -> u16 {
    match e {
        Error::BadRequest(..) => 400,
        Error::Unauthorized(_) => 401,
        Error::Forbidden(..) => 403,
        Error::NotFound(_) => 404,
        Error::Conflict(_) => 409,
        Error::Unavailable(..) => 503,
        Error::Db(_) | Error::Internal(_) => 500,
    }
}

impl Accounts {
    /// Send account events here (rolld: roll-access's audit log).
    pub fn set_audit(&self, audit: Arc<dyn Audit>) {
        *self.audit.write().unwrap_or_else(|e| e.into_inner()) = Some(audit);
    }

    fn audit_hook(&self) -> Option<Arc<dyn Audit>> {
        self.audit.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A person's own security log (their sign-ins and changes to their account), newest first.
    pub fn activity(&self, user: &str, before: Option<i64>, limit: usize) -> Vec<Activity> {
        self.audit_hook().map(|h| h.activity(user, before, limit.clamp(1, 200))).unwrap_or_default()
    }

    pub(crate) fn record(&self, e: AccountEvent) {
        if let Some(a) = self.audit_hook() {
            a.record(e);
        }
    }

    /// A sign-in that ended: well (`user`, `refused: None`; `second_factor`: it still owes one) or
    /// not (the error, and the person when they're known).
    pub(crate) fn record_sign_in(&self, user: Option<&str>, method: &str, refused: Option<&Error>, second_factor: bool, who: &Who) {
        // A whole sign-in (nothing more owed): from somewhere new, the person is mailed.
        if let (Some(u), None, false) = (user, refused, second_factor) {
            self.note_sign_in_place(u, who);
        }
        self.sign_in_event(user.unwrap_or("anonymous"), user, method, refused, second_factor, who);
    }

    /// A person confirmed it's them for sudo mode (or failed to).
    pub(crate) fn record_reauth(&self, user: &str, method: &str, refused: Option<&Error>, who: &Who) {
        self.record(AccountEvent {
            actor: user.to_string(),
            action: "reauth".into(),
            method: "-".into(),
            target: Some(user.to_string()),
            ok: refused.is_none(),
            status: refused.map(status_of).unwrap_or(200),
            ip: who.ip,
            user_agent: who.user_agent.clone(),
            detail: json!({ "method": clean_id(method), "refused": refused.map(Error::redirect_code) }),
        });
    }

    /// A refused code for someone's account (`owner`, when the code was sent to one): in their
    /// security log, by nobody known, as GitHub shows failed attempts.
    pub(crate) fn record_code_refused(&self, owner: Option<&str>, method: &str, refused: &Error, who: &Who) {
        self.sign_in_event("anonymous", owner, method, Some(refused), false, who);
    }

    fn sign_in_event(&self, actor: &str, target: Option<&str>, method: &str, refused: Option<&Error>, second_factor: bool, who: &Who) {
        self.record(AccountEvent {
            actor: actor.to_string(),
            action: "sign_in".into(),
            method: "-".into(),
            target: target.map(str::to_string),
            ok: refused.is_none(),
            status: refused.map(status_of).unwrap_or(200),
            ip: who.ip,
            user_agent: who.user_agent.clone(),
            detail: json!({ "method": clean_id(method), "refused": refused.map(Error::redirect_code), "second_factor": if second_factor { Some("owed") } else { None } }),
        });
    }

    /// What a sign-in made: an account (from an invite, or SSO's provisioning) or a link.
    pub(crate) fn record_new(&self, user: &str, new: &str, method: &str, who: &Who) {
        let (action, actor) = match new {
            "linked" => ("identity.link", user.to_string()),
            _ => ("user.create", if new == "provisioned" { clean_id(method) } else { user.to_string() }),
        };
        self.record(AccountEvent {
            actor,
            action: action.into(),
            method: "-".into(),
            target: Some(user.to_string()),
            ok: true,
            status: 200,
            ip: who.ip,
            user_agent: who.user_agent.clone(),
            detail: json!({ "method": clean_id(method), "via": new }),
        });
    }
}

/// Routes whose handlers record their own events (sign-ins, SCIM), or that only start a flow.
fn recorded_elsewhere(route: &str) -> bool {
    route.starts_with("/v1/accounts/scim/")
        || matches!(
            route,
            "/v1/accounts/email/start" | "/v1/accounts/email/verify" | "/v1/accounts/oauth/{provider}/start" | "/v1/accounts/sso/start" | "/v1/accounts/signin/totp" | "/v1/accounts/signin/recovery" | "/v1/accounts/invites/peek"
                | "/v1/accounts/signin/passkey" | "/v1/accounts/signin/passkey/options" | "/v1/accounts/me/passkeys/options"
                | "/v1/accounts/me/reauth" | "/v1/accounts/me/reauth/email" | "/v1/accounts/me/reauth/passkey/options"
                | "/v1/accounts/sso/{id}/backchannel-logout"
                | "/v1/accounts/signin/reset-request" | "/v1/accounts/reset-requests/{id}/approve" | "/v1/accounts/reset-requests/{id}/deny"
        )
}

/// The allowlisted fields of a JSON body (never anything else of it).
fn fields_of(body: &[u8]) -> Map<String, Value> {
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) else { return Map::new() };
    m.into_iter().filter(|(k, _)| FIELDS.contains(&k.as_str())).collect()
}

/// The client's address on every accounts request (for handlers' events), and an event for each
/// write once it's answered.
pub(crate) async fn layer(State(a): State<Arc<Accounts>>, mut req: Request, next: Next) -> Response {
    let hook = a.audit_hook();
    let ip = hook.as_ref().and_then(|h| h.client_ip(&req));
    if let Some(ip) = ip {
        req.extensions_mut().insert(ClientIp(ip));
    }
    let method = req.method().clone();
    let route = req.extensions().get::<MatchedPath>().map(|m| m.as_str().to_string()).unwrap_or_default();
    let write = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) && !recorded_elsewhere(&route);
    if hook.is_none() || !write {
        return next.run(req).await;
    }
    let actor = match (req.extensions().get::<SignedIn>(), req.extensions().get::<Operator>()) {
        (Some(u), _) => u.user.clone(),
        (None, Some(_)) => "operator".to_string(),
        (None, None) if a.config.open => "operator (open daemon)".to_string(),
        _ => "anonymous".to_string(),
    };
    // The id the route names (`{id}`), decoded as the handler reads it, from the path's segment
    // at the pattern's place; a person's own routes (`/v1/accounts/me…`) are about them.
    let own = route.starts_with("/v1/accounts/me").then(|| actor.clone());
    let target = own.or_else(|| route
        .split('/')
        .zip(req.uri().path().split('/'))
        .find(|(p, _)| p.starts_with('{') && *p != "{provider}")
        .map(|(_, v)| clean_id(&percent_encoding::percent_decode_str(v).decode_utf8_lossy())));
    let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().filter(|c| !c.is_control()).take(120).collect::<String>());
    // The body, for its allowlisted fields (handed on as it came).
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return (axum::http::StatusCode::PAYLOAD_TOO_LARGE, axum::Json(json!({ "error": "the body is too large" }))).into_response();
    };
    let fields = fields_of(&bytes);
    let req = Request::from_parts(parts, Body::from(bytes));
    // In a task of its own: the change and its event happen even if the client goes.
    let task = tokio::spawn(async move {
        let resp = next.run(req).await;
        let status = resp.status();
        // A refusal before anything happened, by nobody: not worth a row (anyone could fill the log).
        if actor == "anonymous" && matches!(status.as_u16(), 401 | 403 | 404 | 405 | 415) {
            return resp;
        }
        // What a create made: the answer's `id`.
        let (resp, made) = if method == Method::POST && status.is_success() {
            let (parts, body) = resp.into_parts();
            match axum::body::to_bytes(body, MAX_BODY).await {
                Ok(b) => {
                    let id = serde_json::from_slice::<Value>(&b).ok().and_then(|v| v.get("id").and_then(Value::as_str).map(clean_id));
                    (Response::from_parts(parts, Body::from(b)), id)
                }
                Err(_) => (Response::from_parts(parts, Body::empty()), None),
            }
        } else {
            (resp, None)
        };
        let mut detail = Map::new();
        if !fields.is_empty() {
            detail.insert("set".into(), Value::Object(fields));
        }
        if let Some(m) = &made {
            detail.insert("made".into(), json!(m));
        }
        a.record(AccountEvent {
            actor,
            action: format!("{method} {route}"),
            method: method.to_string(),
            target: target.or(made),
            ok: status.is_success() || status.is_redirection(),
            status: status.as_u16(),
            ip,
            user_agent: ua,
            detail: if detail.is_empty() { Value::Null } else { Value::Object(detail) },
        });
        resp
    });
    task.await.unwrap_or_else(|_| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "error": "internal error" }))).into_response())
}
