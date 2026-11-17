//! People's accounts in the audit log (docs/accounts.md, "Audit"): who signed in and how (and
//! who failed to), and every change to accounts, invites, SSO connections and SCIM, as Okta's
//! System Log and GitHub's audit log keep them. rolld writes them to roll-access's audit log (its
//! page, filters and SIEM sinks); roll-accounts only says what happened.
//!
//! - Every write to an accounts route is recorded after it's answered: its method and route, the
//!   id it names, who made it (a person's id, `operator`, `anonymous`), its status, the client's
//!   address and user agent. Never a body: no code, token, secret or password is ever in an event.
//! - Sign-ins (email code, GitHub, Google, SSO, a second factor) and SCIM's changes are recorded
//!   by their handlers, which know the person and the method.

use crate::Accounts;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::Response;
use roll_auth::{ClientIp, Operator, SignedIn};
use serde::Serialize;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Arc;

/// Where account events go (rolld: roll-access's audit log), and how a request's client address
/// is read (its trusted proxies).
pub trait Audit: Send + Sync {
    fn record(&self, e: AccountEvent);
    fn client_ip(&self, req: &Request) -> Option<IpAddr>;
}

/// One thing that happened to accounts.
#[derive(Clone, Debug, Serialize)]
pub struct AccountEvent {
    /// A person's id, `operator`, `anonymous`, or `scim:<connection>`.
    pub actor: String,
    /// `sign_in`, `scim.create`, or the request's `METHOD /route`.
    pub action: String,
    pub method: String,
    /// The person or object it's about.
    pub target: Option<String>,
    pub ok: bool,
    pub status: u16,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    /// What else matters (a sign-in's method, why it was refused, SCIM's change): never a secret.
    pub detail: Value,
}

impl Accounts {
    /// Send account events here (rolld: roll-access's audit log).
    pub fn set_audit(&self, audit: Arc<dyn Audit>) {
        *self.audit.write().unwrap_or_else(|e| e.into_inner()) = Some(audit);
    }

    fn audit_hook(&self) -> Option<Arc<dyn Audit>> {
        self.audit.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn record(&self, e: AccountEvent) {
        if let Some(a) = self.audit_hook() {
            a.record(e);
        }
    }

    /// A sign-in that ended, well (`user`) or not (`refused`: the error's code).
    pub(crate) fn record_sign_in(&self, user: Option<&str>, method: &str, refused: Option<&str>, who: &Who) {
        self.record(AccountEvent {
            actor: user.unwrap_or("anonymous").to_string(),
            action: "sign_in".into(),
            method: "-".into(),
            target: user.map(str::to_string),
            ok: refused.is_none(),
            status: if refused.is_none() { 200 } else { 403 },
            ip: who.ip,
            user_agent: who.user_agent.clone(),
            detail: json!({ "method": method, "refused": refused }),
        });
    }
}

/// Where a request came from, for its events.
#[derive(Clone, Debug, Default)]
pub struct Who {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

impl Who {
    pub(crate) fn of(ip: Option<axum::Extension<ClientIp>>, headers: &axum::http::HeaderMap) -> Who {
        let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().filter(|c| !c.is_control()).take(120).collect::<String>());
        Who { ip: ip.map(|axum::Extension(ClientIp(ip))| ip), user_agent: ua }
    }
}

/// Routes whose handlers record their own events (sign-ins, SCIM), or that only start a flow.
fn recorded_elsewhere(route: &str) -> bool {
    route.starts_with("/v1/accounts/scim/")
        || matches!(
            route,
            "/v1/accounts/email/start" | "/v1/accounts/email/verify" | "/v1/accounts/oauth/{provider}/start" | "/v1/accounts/sso/start" | "/v1/accounts/signin/totp" | "/v1/accounts/signin/recovery" | "/v1/accounts/invites/peek"
        )
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
    // The id the route names (`{id}`), from the path's segment at the pattern's place.
    let target = route.split('/').zip(req.uri().path().split('/')).find(|(p, _)| p.starts_with('{') && *p != "{provider}").map(|(_, v)| v.to_string());
    let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().filter(|c| !c.is_control()).take(120).collect::<String>());
    let resp = next.run(req).await;
    let status = resp.status();
    a.record(AccountEvent {
        actor,
        action: format!("{method} {route}"),
        method: method.to_string(),
        target,
        ok: status.is_success() || status.is_redirection(),
        status: status.as_u16(),
        ip,
        user_agent: ua,
        detail: Value::Null,
    });
    resp
}
