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
//! - [`Access::User`] routes take the operator or any signed-in user; the product scopes what a
//!   user sees (workspace projects).
//! - Everything else, and every path no route matches, is [`Access::Operator`]: the token or 401.
//!
//! People sign in with a session cookie (docs/accounts.md). The gate resolves it through
//! [`Sessions`] on every request (nothing is cached), checks CSRF on writes, and puts
//! [`SignedIn`] in the request's extensions; an admin's session also carries [`Operator`]. A
//! signed-in user who isn't an admin gets 403 on [`Access::Operator`] routes (and on
//! [`Access::Credential`] routes without the product's credential).
//!
//! Without a token (a daemon on loopback, local development) the gate lets everything through
//! and products behave as they always have; a session cookie still says who is signed in.
//!
//! Scoped tokens (docs/permissions.md) are an [`Authorizer`]'s: a request bearing one goes to it
//! whatever the route and whether or not the operator's token is set, so a token's limits hold
//! everywhere. It sends allowed requests on with [`Operator`] and a [`Grant`] in their extensions.

pub mod scope;
pub mod totp;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer, ExposeHeaders};

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

/// A grant's token as a request presents it: `Authorization: Bearer <token>`, or as the password
/// of `Basic` (what git sends). Only grant tokens are taken from `Basic`.
pub fn grant_credential(headers: &HeaderMap) -> Option<String> {
    if let Some(t) = bearer(headers) {
        return Some(t.to_string());
    }
    use base64::Engine;
    let (scheme, value) = headers.get(header::AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(value.trim()).ok()?;
    let (_, password) = String::from_utf8(decoded).ok()?.split_once(':').map(|(u, p)| (u.to_string(), p.to_string()))?;
    (is_grant_token(&password) || password.starts_with("rlf_")).then_some(password)
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
    /// A signed-in user's own token (`usr_…`): it acts as that user (docs/permissions.md, "Users'
    /// own tokens"), never as more. `None`: the operator's (or an admin's from before).
    pub user: Option<Arc<str>>,
    /// What `scope` doesn't allow asks a person instead of being refused.
    pub ask_otherwise: bool,
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
        f.debug_struct("Grant").field("id", &self.id).field("name", &self.name).field("kind", &self.kind).field("user", &self.user).finish()
    }
}

/// In an allowed grant's request: the client's address as the gate saw it. Requests a product
/// makes on the caller's behalf (roll code's agents relay) carry it on, so an IP allowlist holds
/// for them too. Set only by the gate: extensions never come from the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub std::net::IpAddr);

/// Prefixes of the tokens a grant opens requests with: scoped tokens and OAuth access tokens.
pub const GRANT_PREFIXES: [&str; 2] = ["rlt_", "rlo_"];

/// Whether `token` is a grant's (it's decided by the [`Authorizer`], never by a product). A
/// request a node forwards over a signed hop already passed that node's gate: the node it lands
/// on may take such a token as decided.
pub fn is_grant_token(token: &str) -> bool {
    GRANT_PREFIXES.iter().any(|p| token.starts_with(p))
}

/// Grants looked up outside a request (a product acting later on a grant's behalf, like secrets'
/// syncs): the grant as it is now, `None` once it's revoked, expired or gone. Never cache it.
pub trait Grants: Send + Sync + 'static {
    fn grant(&self, id: &str) -> Option<Grant>;
}

/// The evaluator for scoped tokens (`roll-access`).
pub trait Authorizer: Send + Sync + 'static {
    /// Whether `token` is one of its own (by its prefix): then it alone decides the request.
    fn claims(&self, token: &str) -> bool;
    /// Decide the request: answer it, or run it through `next`.
    fn handle(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send + 'static>>;
}

/// A signed-in user, in a request's extensions: the gate resolved the session cookie. Products
/// read who it is from here (roll code: the user's owner; workspace: the user's projects).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedIn {
    /// `usr_…`
    pub user: String,
    pub handle: String,
    /// An admin acts as the operator (the request carries [`Operator`] too).
    pub admin: bool,
    /// The user's roll code owner (`own_…`), once it exists.
    pub code_owner: Option<String>,
    /// The session's public id (`ses_…`).
    pub session: String,
}

/// A session that still owes its second factor, in a request's extensions: only the
/// second-factor routes accept it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondFactorOwed {
    pub user: String,
    pub session: String,
}

/// What a session cookie's token stands for.
#[derive(Clone, Debug)]
pub enum Session {
    Full(SignedIn),
    SecondFactor(SecondFactorOwed),
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resolves session cookies (`roll-accounts`): one lookup per request, never cached, so
/// revocation is immediate. `Err` is a database failure (the gate answers 503).
pub trait Sessions: Send + Sync + 'static {
    /// The cookie's name: `__Host-roll_session`, or `roll_session` on plain-HTTP local setups.
    fn cookie(&self) -> &str;
    fn resolve<'a>(&'a self, token: &'a str) -> BoxFuture<'a, Result<Option<Session>, String>>;
    /// A user as they are now (`session` left empty), for a token of theirs: `None` once they're
    /// disabled or gone. Never cached.
    fn person<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Option<SignedIn>, String>> {
        let _ = user;
        Box::pin(async { Ok(None) })
    }
    /// Mail a person about something of theirs (a token of theirs used from a new network,
    /// revoked as leaked, or about to expire). Nothing when they're disabled or gone, or mail is
    /// off. `text` holds no secret.
    fn tell<'a>(&'a self, user: &'a str, subject: &'a str, text: &'a str) -> BoxFuture<'a, Result<(), String>> {
        let _ = (user, subject, text);
        Box::pin(async { Ok(()) })
    }
}

/// In the request an [`Authorizer`] decides: who the gate's rules let call its route, so a user's
/// token reaches only what that user's session would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteAccess(pub Access);

/// The gate's rules, for an [`Authorizer`] to class a route other than the one matched (the request
/// a signed hop carries).
#[derive(Clone)]
pub struct RouteRules(pub Arc<Gate>);

impl RouteRules {
    pub fn access(&self, route: &str) -> Access {
        self.0.access(route)
    }
}

/// The CSRF header a write with a session cookie carries (docs/accounts.md, "CSRF").
pub const CSRF_HEADER: &str = "x-roll-csrf";

/// The CSRF token of a session: derived from its token, so it needs no storage and changes with
/// every session.
pub fn csrf_token(session_token: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"roll-csrf:");
    h.update(session_token.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

/// The value of cookie `name` in the request's `Cookie` headers.
pub fn cookie<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// Who may call a route; the text says why (or which credential).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Public(&'static str),
    Product(&'static str),
    Credential(&'static str),
    /// The operator or any signed-in user; the product scopes what each sees.
    User(&'static str),
    Operator,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Access::Public(why) => write!(f, "public: {why}"),
            Access::Product(why) => write!(f, "product: {why}"),
            Access::Credential(what) => write!(f, "operator or {what}"),
            Access::User(why) => write!(f, "user: {why}"),
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
    sessions: Option<Arc<dyn Sessions>>,
    /// Origins a write with a session cookie may come from (the dashboard's, and the API's own).
    origins: Origins,
    own_origin: Option<HeaderValue>,
}

impl Gate {
    pub fn new(token: Option<AdminToken>, rules: Vec<Rule>) -> Arc<Self> {
        Arc::new(Self { token, rules, authorizer: OnceLock::new(), sessions: None, origins: Origins::List(Vec::new()), own_origin: None })
    }

    /// Scoped tokens' evaluator, set once when it's ready (before serving).
    pub fn set_authorizer(&self, a: Arc<dyn Authorizer>) {
        let _ = self.authorizer.set(a);
    }

    /// A gate that also signs people in with session cookies. `origins`: the dashboard's
    /// (`ROLL_DASHBOARD_ORIGIN`); `own`: the API's public URL, whose origin may send writes too.
    pub fn with_sessions(token: Option<AdminToken>, rules: Vec<Rule>, sessions: Arc<dyn Sessions>, origins: Origins, own: &str) -> Arc<Self> {
        let own_origin = origin_of(own).and_then(|o| HeaderValue::from_str(&o).ok());
        Arc::new(Self { token, rules, authorizer: OnceLock::new(), sessions: Some(sessions), origins, own_origin })
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
    // A public route is public whatever a request carries (a grant holder opening a shared link).
    let public = req.extensions().get::<MatchedPath>().is_some_and(|m| matches!(gate.access(m.as_str()), Access::Public(_)));
    // A grant, or something that looks like one, never passes as anything else (nor with a cookie).
    if !public
        && let (Some(a), Some(p)) = (gate.authorizer.get(), grant_credential(req.headers()))
        && a.claims(&p)
    {
        let access = req.extensions().get::<MatchedPath>().map_or(Access::Operator, |m| gate.access(m.as_str()));
        req.extensions_mut().insert(RouteAccess(access));
        req.extensions_mut().insert(RouteRules(gate.clone()));
        return a.handle(req, next).await;
    }
    // A request with an `Authorization` header is judged by it alone: the cookie is ignored.
    let authorization = req.headers().contains_key(header::AUTHORIZATION);
    let mut signed_in = None;
    if !authorization
        && let Some(sessions) = &gate.sessions
        && let Some(token) = cookie(req.headers(), sessions.cookie())
    {
        let session = match sessions.resolve(token).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "sessions: lookup failed");
                return error(StatusCode::SERVICE_UNAVAILABLE, "sessions are unavailable; try again", "unavailable");
            }
        };
        if let Some(session) = session {
            if !safe(req.method()) && !gate.csrf_ok(req.headers(), token) {
                return error(StatusCode::FORBIDDEN, "this request needs the X-Roll-CSRF header from GET /v1/accounts/session", "csrf");
            }
            match session {
                Session::Full(s) => {
                    if s.admin {
                        req.extensions_mut().insert(Operator);
                    }
                    signed_in = Some(s.admin);
                    req.extensions_mut().insert(s);
                }
                Session::SecondFactor(s) => {
                    req.extensions_mut().insert(s);
                }
            }
        }
    }
    if gate.token.is_none() || signed_in == Some(true) {
        return next.run(req).await;
    }
    let access = req.extensions().get::<MatchedPath>().map_or(Access::Operator, |m| gate.access(m.as_str()));
    match access {
        Access::Public(_) | Access::Product(_) => next.run(req).await,
        Access::Credential(_) if authorization => next.run(req).await,
        Access::User(_) if signed_in.is_some() => next.run(req).await,
        _ if signed_in.is_some() => error(
            StatusCode::FORBIDDEN,
            "your account can use roll code, agents and projects; this API is for roll's admins for now",
            "operator_only",
        ),
        _ => unauthorized(),
    }
}

fn safe(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

impl Gate {
    /// The CSRF header matches the session, and the origin (when sent) is one we serve.
    fn csrf_ok(&self, headers: &HeaderMap, session_token: &str) -> bool {
        let header_ok = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()).is_some_and(|v| constant_eq(v.trim().as_bytes(), csrf_token(session_token).as_bytes()));
        let origin_ok = match headers.get(header::ORIGIN) {
            None => true,
            Some(o) => {
                self.own_origin.as_ref() == Some(o)
                    || match &self.origins {
                        Origins::Local => is_local(o),
                        Origins::List(l) => l.iter().any(|a| a.as_bytes().eq_ignore_ascii_case(o.as_bytes())),
                    }
            }
        };
        header_ok && origin_ok
    }
}

/// `scheme://host[:port]` of a URL.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

fn error(status: StatusCode, msg: &str, code: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": msg, "code": code }))).into_response()
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

/// Response headers the dashboard may read besides the CORS-safelisted ones (with credentials
/// allowed, `*` isn't an option).
const EXPOSED: [&str; 9] = ["location", "retry-after", "www-authenticate", "content-disposition", "etag", "last-modified", "x-roll-changeset", "mcp-session-id", "x-request-id"];

/// One CORS policy for every control-plane API: these origins, any method and request header,
/// with credentials (the session cookie, docs/accounts.md).
pub fn cors(origins: &Origins) -> CorsLayer {
    let allow = match origins {
        Origins::Local => AllowOrigin::predicate(|o, _| is_local(o)),
        Origins::List(list) => AllowOrigin::list(list.clone()),
    };
    CorsLayer::new()
        .allow_origin(allow)
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers(ExposeHeaders::list(EXPOSED.map(axum::http::HeaderName::from_static)))
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

    #[test]
    fn cookies_and_origins() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1; roll_session=tok; b=2"));
        assert_eq!(cookie(&h, "roll_session"), Some("tok"));
        assert_eq!(cookie(&h, "roll"), None);
        assert_eq!(origin_of("https://API.roll.sh/v1"), Some("https://api.roll.sh".into()));
        assert_ne!(csrf_token("a"), csrf_token("b"));
    }
}
