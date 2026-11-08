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

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::fmt;
use std::sync::Arc;
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

    /// The token itself, for a product that checks it on its own (roll code's admin token).
    pub fn reveal(&self) -> &str {
        &self.0
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
}

impl Gate {
    pub fn new(token: Option<AdminToken>, rules: Vec<Rule>) -> Arc<Self> {
        Arc::new(Self { token, rules })
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
    let Some(token) = &gate.token else { return next.run(req).await };
    let access = req.extensions().get::<MatchedPath>().map_or(Access::Operator, |m| gate.access(m.as_str()));
    if let Access::Public(_) = access {
        return next.run(req).await;
    }
    if bearer(req.headers()).is_some_and(|t| token.matches(t)) {
        req.extensions_mut().insert(Operator);
        return next.run(req).await;
    }
    match access {
        Access::Product(_) => next.run(req).await,
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
    /// Comma-separated origins (`https://roll.cloud,https://staging.roll.cloud`).
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
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn app() -> Router {
        let rules = vec![
            Rule { route: "/health", access: Access::Public("load balancers") },
            Rule { route: "/v1/code", access: Access::Product("roll code's own rules") },
            Rule { route: "/v1/storage", access: Access::Credential("a console session") },
        ];
        let gate = Gate::new(Some(AdminToken::new(TOKEN).unwrap()), rules);
        let op = |req: Request| async move { if Operator::on(req.extensions()) { "operator" } else { "caller" } };
        Router::new()
            .route("/health", get(op))
            .route("/v1/code/repos/{id}", get(op))
            .route("/v1/codex", get(op))
            .route("/v1/storage/buckets", get(op))
            .route("/v1/projects", get(op))
            .layer(axum::middleware::from_fn_with_state(gate, check))
    }

    async fn call(path: &str, auth: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::get(path);
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let res = app().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn routes_take_what_their_access_says() {
        let op = format!("Bearer {TOKEN}");
        assert_eq!(call("/health", None).await, (StatusCode::OK, "caller".into()));
        assert_eq!(call("/v1/code/repos/x", None).await, (StatusCode::OK, "caller".into()), "the product decides");
        assert_eq!(call("/v1/codex", None).await.0, StatusCode::UNAUTHORIZED, "a prefix is a whole segment");
        assert_eq!(call("/v1/storage/buckets", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call("/v1/storage/buckets", Some("Bearer session")).await, (StatusCode::OK, "caller".into()), "the product verifies its own");
        assert_eq!(call("/v1/projects", Some("Bearer session")).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call("/v1/projects", Some(&format!("Bearer {TOKEN}x"))).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call("/nothing/here", None).await.0, StatusCode::UNAUTHORIZED, "no route says nothing either");
        for path in ["/v1/projects", "/v1/storage/buckets", "/v1/code/repos/x"] {
            assert_eq!(call(path, Some(&op)).await, (StatusCode::OK, "operator".into()), "{path}");
        }
        assert_eq!(call("/nothing/here", Some(&op)).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cors_answers_only_the_dashboard() {
        let origins = Origins::parse("https://roll.cloud").unwrap();
        let app = Router::new().route("/v1/projects", get(|| async { "ok" })).layer(cors(&origins));
        let preflight = |origin: &str| {
            Request::options("/v1/projects")
                .header(header::ORIGIN, origin)
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
                .body(Body::empty())
                .unwrap()
        };
        let res = app.clone().oneshot(preflight("https://roll.cloud")).await.unwrap();
        assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "https://roll.cloud");
        assert!(res.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS].to_str().unwrap().contains("authorization"));
        assert!(!res.headers().contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS));
        let res = app.clone().oneshot(preflight("https://evil.example")).await.unwrap();
        assert!(!res.headers().contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN), "a foreign origin gets no CORS grant");

        let local = Router::new().route("/v1/projects", get(|| async { "ok" })).layer(cors(&Origins::Local));
        for (origin, ok) in [("http://localhost:3000", true), ("http://127.0.0.1:5173", true), ("http://[::1]:3000", true), ("http://localhost.evil.example", false), ("http://evil.example:3000", false)] {
            let res = local.clone().oneshot(preflight(origin)).await.unwrap();
            assert_eq!(res.headers().contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN), ok, "{origin}");
        }
        assert!(Origins::parse("https://roll.cloud/path").is_err());
        assert!(Origins::parse("*").is_err());
    }

    #[test]
    fn the_token_is_long_and_never_printed() {
        assert!(AdminToken::new("short").is_err());
        assert!(AdminToken::new("0123456789abcdef 0123456789abcdef").is_err());
        let t = AdminToken::new(TOKEN).unwrap();
        assert!(t.matches(TOKEN) && !t.matches("0123456789abcdef0123456789abcdeX") && !t.matches(""));
        assert!(!format!("{t:?}").contains("0123"));
    }
}
