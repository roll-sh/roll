//! Who may call roll's control-plane APIs.
//!
//! rolld merges every product's HTTP control plane onto one router and puts [`check`] in front of
//! all of it, once. Each route has an [`Access`] (rolld lists them in one place:
//! `crates/rolld/src/auth.rs`):
//!
//! - [`Access::Public`] routes pass as they are.
//! - The operator's token (`ROLL_ADMIN_TOKEN`, as `Authorization: Bearer`) opens every other
//!   route, and the request then carries [`Operator`] in its extensions: a product with
//!   credentials of its own takes that as its operator without ever seeing the token.
//! - [`Access::Credential`] routes also take the product's own credentials (storage console
//!   sessions, tasks API keys, stream API keys): a request with an `Authorization` header goes on
//!   to the product, which verifies it. Without one it's a 401.
//! - [`Access::Product`] routes decide everything themselves, anonymous callers included (roll
//!   code's owners, tokens and public repositories; signed internal hops).
//! - Everything else, and every path no route matches, is [`Access::Operator`]: the token or 401.
//!
//! Without a token (a daemon on loopback, local development) the gate lets everything through
//! and products behave as they always have.
//!
//! Scoped tokens (docs/permissions.md) are an [`Authorizer`]'s: a request bearing one goes to it
//! whatever the route and whether or not the operator's token is set, so a token's limits hold
//! everywhere. It sends allowed requests on with [`Operator`] and a [`Grant`] in their extensions.

pub mod scope;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower_http::cors::{AllowHeaders, AllowOrigin, Any, CorsLayer};

/// The operator's token. `Debug` never shows it.
#[derive(Clone)]
pub struct AdminToken(Arc<str>);

impl AdminToken {
    pub const MIN_LEN: usize = 32;

    pub fn new(token: &str) -> Result<Self, &'static str> {
        let t = token.trim();
        if t.len() < Self::MIN_LEN {
            return Err("must be at least 32 characters");
        }
        if !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("must be printable ASCII without spaces");
        }
        Ok(Self(t.into()))
    }

    /// Whether `presented` is this token, in time that doesn't depend on where they differ.
    pub fn matches(&self, presented: &str) -> bool {
        constant_eq(presented.as_bytes(), self.0.as_bytes())
    }
}

impl fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminToken(…)")
    }
}

/// Equal, comparing every byte (only a difference in length returns early).
pub fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token in `Authorization: Bearer <token>`.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers.get(header::AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    Some(token.trim()).filter(|t| scheme.eq_ignore_ascii_case("bearer") && !t.is_empty())
}

/// In a request's extensions when it carried the operator's token.
#[derive(Clone, Copy, Debug)]
pub struct Operator;

impl Operator {
    pub fn on(extensions: &axum::http::Extensions) -> bool {
        extensions.get::<Operator>().is_some()
    }
}

/// In a request's extensions when a scoped token or an OAuth connection made it (with
/// [`Operator`]: the request goes on as the operator's, narrowed by the gate). Products that need
/// a second check inside one request ask [`Grant::allows`]; the operator's requests have none.
#[derive(Clone)]
pub struct Grant {
    /// `tok_…`: the stable principal id.
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub kind: GrantKind,
    pub scope: Arc<scope::Scope>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantKind {
    Token,
    Oauth,
}

impl Grant {
    pub fn on(extensions: &axum::http::Extensions) -> Option<&Grant> {
        extensions.get::<Grant>()
    }

    /// What the grant says about `action` on one resource.
    pub fn allows(&self, resource: &scope::Resource, action: scope::Action) -> scope::Outcome {
        self.scope.outcome(&scope::Target::One(resource.clone()), action)
    }
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant").field("id", &self.id).field("name", &self.name).field("kind", &self.kind).finish()
    }
}

/// The evaluator for scoped tokens (`roll-access`).
pub trait Authorizer: Send + Sync + 'static {
    /// Whether `token` is one of its own (by its prefix): then it alone decides the request.
    fn claims(&self, token: &str) -> bool;
    /// Decide the request: answer it, or run it through `next`.
    fn handle(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send + 'static>>;
}

/// Who may call a route; the text says why (or which credential).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Public(&'static str),
    Product(&'static str),
    Credential(&'static str),
    Operator,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Access::Public(why) => write!(f, "public: {why}"),
            Access::Product(why) => write!(f, "product: {why}"),
            Access::Credential(what) => write!(f, "operator or {what}"),
            Access::Operator => f.write_str("operator"),
        }
    }
}

/// `route` and every route under it (`/v1/code` covers `/v1/code/repos`, not `/v1/codex`).
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub route: &'static str,
    pub access: Access,
}

pub struct Gate {
    token: Option<AdminToken>,
    rules: Vec<Rule>,
    authorizer: OnceLock<Arc<dyn Authorizer>>,
}

impl Gate {
    pub fn new(token: Option<AdminToken>, rules: Vec<Rule>) -> Arc<Self> {
        Arc::new(Self { token, rules, authorizer: OnceLock::new() })
    }

    /// Scoped tokens' evaluator, set once when it's ready (before serving).
    pub fn set_authorizer(&self, a: Arc<dyn Authorizer>) {
        let _ = self.authorizer.set(a);
    }

    /// The access of a route pattern (as the router has it): its longest matching rule's, else
    /// the operator's.
    pub fn access(&self, route: &str) -> Access {
        self.rules
            .iter()
            .filter(|r| route == r.route || (route.starts_with(r.route) && (r.route.ends_with('/') || route[r.route.len()..].starts_with('/'))))
            .max_by_key(|r| r.route.len())
            .map_or(Access::Operator, |r| r.access)
    }

    /// Whether requests are checked at all (there is a token).
    pub fn enforcing(&self) -> bool {
        self.token.is_some()
    }
}

/// The middleware: `router.layer(axum::middleware::from_fn_with_state(gate, roll_auth::check))`.
/// It reads the matched route, so it goes on the router whose routes it guards.
pub async fn check(State(gate): State<Arc<Gate>>, mut req: Request, next: Next) -> Response {
    let presented = bearer(req.headers());
    if let (Some(token), Some(p)) = (&gate.token, presented)
        && token.matches(p)
    {
        req.extensions_mut().insert(Operator);
        return next.run(req).await;
    }
    // A grant, or something that looks like one, never passes as anything else.
    if let (Some(a), Some(p)) = (gate.authorizer.get(), presented)
        && a.claims(p)
    {
        return a.handle(req, next).await;
    }
    let Some(_) = &gate.token else { return next.run(req).await };
    let access = req.extensions().get::<MatchedPath>().map_or(Access::Operator, |m| gate.access(m.as_str()));
    match access {
        Access::Public(_) | Access::Product(_) => next.run(req).await,
        Access::Credential(_) if req.headers().contains_key(header::AUTHORIZATION) => next.run(req).await,
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    let body = serde_json::json!({ "error": "this API needs a token: Authorization: Bearer <ROLL_ADMIN_TOKEN>", "code": "unauthorized" });
    let mut res = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"roll\""));
    res
}

/// The browser origins the control-plane APIs answer (`ROLL_DASHBOARD_ORIGIN`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origins {
    /// Any port on localhost, 127.0.0.1 or [::1]: a daemon on loopback, for the local dashboard.
    Local,
    List(Vec<HeaderValue>),
}

impl Origins {
    /// Comma-separated origins (`https://roll.sh,https://staging.roll.sh`).
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut out = Vec::new();
        for o in list.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            let o = o.trim_end_matches('/').to_ascii_lowercase();
            let Some((scheme, host)) = o.split_once("://") else { return Err(format!("{o} isn't an origin (scheme://host[:port])")) };
            if !matches!(scheme, "http" | "https") || host.is_empty() || host.contains(['/', '?', '#', '@', '*']) {
                return Err(format!("{o} isn't an origin (scheme://host[:port])"));
            }
            out.push(HeaderValue::from_str(&o).map_err(|_| format!("{o} isn't an origin"))?);
        }
        Ok(Origins::List(out))
    }
}

fn is_local(origin: &HeaderValue) -> bool {
    let Ok(o) = origin.to_str() else { return false };
    let Some(rest) = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) else { return false };
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|host| match rest.strip_prefix(host) {
        Some("") => true,
        Some(port) => port.strip_prefix(':').is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    })
}

/// One CORS policy for every control-plane API: these origins, any method and request header,
/// every response header readable, no cookies (the dashboard sends tokens, not credentials).
pub fn cors(origins: &Origins) -> CorsLayer {
    let allow = match origins {
        Origins::Local => AllowOrigin::predicate(|o, _| is_local(o)),
        Origins::List(list) => AllowOrigin::list(list.clone()),
    };
    CorsLayer::new()
        .allow_origin(allow)
        .allow_methods([Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers(Any)
        .max_age(Duration::from_secs(3600))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loopback default can't be met by a look-alike host, and the token never prints.
    #[test]
    fn local_origins_and_the_token() {
        for (origin, ok) in [
            ("http://localhost:3000", true),
            ("http://127.0.0.1", true),
            ("http://[::1]:5173", true),
            ("http://localhost.evil.example", false),
            ("http://localhost:3000.evil.example", false),
            ("http://127.0.0.1@evil.example", false),
        ] {
            assert_eq!(is_local(&HeaderValue::from_static(origin)), ok, "{origin}");
        }
        assert!(Origins::parse("https://roll.sh/path").is_err() && Origins::parse("*").is_err());
        assert!(AdminToken::new("short").is_err());
        let t = AdminToken::new("0123456789abcdef0123456789abcdef").unwrap();
        assert!(t.matches("0123456789abcdef0123456789abcdef") && !t.matches("0123456789abcdef0123456789abcdeX"));
        assert!(!format!("{t:?}").contains("0123"));
    }
}
