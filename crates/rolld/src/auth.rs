//! Who may call the control-plane API rolld serves (every product's routes, merged): the
//! operator's token, `ROLL_ADMIN_TOKEN`, opens everything; [`RULES`] is the one list of routes
//! that are public or check their own credentials, and why. Every other route, including any a
//! product adds later, needs the token. The check itself is `roll_auth`.
//!
//! - On loopback the token is optional: without one, nothing is checked (local development).
//! - Off loopback, or with `ROLL_EDGE=1`, rolld refuses to start without one.
//! - CORS answers only the dashboard's origins (`ROLL_DASHBOARD_ORIGIN`; on loopback, by default,
//!   any localhost port), with credentials: people sign in with a session cookie
//!   (docs/accounts.md), which the gate resolves on every request. An admin's session opens what
//!   the token does; anyone else's opens roll code, the agents and their own projects, and gets
//!   403 on the rest.

use crate::Args;
use roll_auth::{Access, AdminToken, Gate, Origins, Rule};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Routes that don't need the operator's token, longest match wins (`/v1/code` covers everything
/// under it). Everything not here is the operator's.
pub const RULES: &[Rule] = &[
    // OAuth for MCP clients (docs/mcp.md, "Authorization"): discovery, registration, the token
    // endpoint and revocation are public by the spec; they're merged in front of the gate.
    Rule { route: "/.well-known", access: Access::Public("OAuth discovery for MCP clients") },
    Rule { route: "/v1/access/oauth/register", access: Access::Public("OAuth dynamic client registration (public clients, PKCE)") },
    Rule { route: "/v1/access/oauth/token", access: Access::Public("OAuth token endpoint: codes and refresh tokens are the credentials") },
    Rule { route: "/v1/access/oauth/revoke", access: Access::Public("OAuth token revocation") },
    Rule { route: "/v1/access/leaks/github", access: Access::Public("GitHub's secret scanning reports: GitHub's signature is the credential") },
    // roll's MCP server answers 401 itself, naming the OAuth metadata; grants are decided by the
    // evaluator before it, and each tool call goes through the gate again.
    Rule { route: "/v1/mcp", access: Access::Product("MCP: 401 with OAuth discovery without a token") },
    // Health: load balancers and the dashboard's status; nothing a tenant owns.
    Rule { route: "/v1/health", access: Access::Public("health: Postgres control plane version and status") },
    Rule { route: "/v1/code/health", access: Access::Public("health: load balancers") },
    Rule { route: "/v1/tasks/health", access: Access::Public("health: tasks node status") },
    Rule { route: "/v1/queues/health", access: Access::Public("health: queues node status") },
    Rule { route: "/healthz", access: Access::Public("health: load balancers") },
    // Version, node and which products are on: the dashboard's sidebar reads it before sign-in.
    // It (and the 404s of products that are off) is merged in front of the gate in main.rs.
    Rule { route: "/v1/info", access: Access::Public("version, node and products on, for the dashboard before sign-in") },
    // roll code has owners and tokens of its own (docs/code/http-api.md "Auth"): public repos and
    // pages read anonymously, git per its own rules, GitHub/Slack/Discord/Linear webhooks verify
    // their signatures, agents' device login and machine links use their own tokens. It takes the
    // operator's token as its admin.
    Rule { route: "/v1/code", access: Access::Product("roll code: owners, tokens, public repos, signed webhooks, device login") },
    Rule { route: "/git", access: Access::Product("roll code's git smart HTTP: its tokens, public repos") },
    // roll apps' deploy hooks: the secret is the URL's last segment (`rlk_…`, a hash kept), for CI
    // and registries' webhooks that send no header; anything but a live hook is a 404, and each
    // deploy is in the service's activity with the caller's address (docs/apps/README.md).
    Rule { route: "/v1/apps/hooks", access: Access::Product("roll apps' deploy hooks: the secret in the URL") },
    // Accounts: sign-in is public; the rest checks the session, an admin's, or the operator's
    // token (docs/accounts.md).
    Rule { route: "/v1/accounts", access: Access::Product("accounts: sign-in, then the session decides") },
    // Workspace projects: the operator and every signed-in user, each seeing their own.
    Rule { route: "/v1/workspace", access: Access::User("workspace projects: a user's own") },
    // Node-to-node hops are signed with the cluster secret; computes fetch WAL with their token.
    Rule { route: "/internal", access: Access::Product("internal: signed hops and the WAL token") },
    // Storage: IAM users signed in to the dashboard (console sessions) act as themselves.
    Rule { route: "/v1/storage/console/signin", access: Access::Public("storage sign-in: checks the IAM user's password and MFA") },
    Rule { route: "/v1/storage/console/session", access: Access::Product("storage: who a console session is, for that session's token only") },
    Rule { route: "/v1/storage", access: Access::Credential("a storage console session") },
    // Tasks: its own token (ROLL_TASKS_TOKEN) and API keys.
    Rule { route: "/v1/tasks", access: Access::Credential("a tasks token or API key") },
    // Hosted tenants (`/t/<tenant>/v1/tasks`) take only their own API keys; the tasks token
    // manages them (`/_roll/tenants`).
    Rule { route: "/t/{tenant}/v1/tasks", access: Access::Credential("the tenant's tasks API key") },
    Rule { route: "/_roll/tenants", access: Access::Credential("the tasks token") },
    // Streams' HTTP data plane and Confluent REST v3: the stream's API keys.
    Rule { route: "/v1/streams/{s}/topics/{t}/records", access: Access::Credential("a stream API key") },
    Rule { route: "/v1/streams/{s}/topics/{t}/partitions/{p}/records", access: Access::Credential("a stream API key") },
    Rule { route: "/v1/streams/{s}/topics/{t}/tail", access: Access::Credential("a stream API key") },
    Rule { route: "/v1/streams/{s}/groups/{g}/commit", access: Access::Credential("a stream API key") },
    Rule { route: "/v1/streams/{s}/groups/{g}/consumers/{c}", access: Access::Credential("a stream API key") },
    Rule { route: "/kafka/v3", access: Access::Credential("a stream API key") },
];

/// The local dashboard (`vite dev`, `pnpm start`) and the desktop app run on these.
const LOCAL_HOSTS: &str = "localhost, 127.0.0.1 or [::1]";

pub struct Auth {
    token: Option<AdminToken>,
    gate: Arc<Gate>,
    origins: Origins,
}

impl Auth {
    /// Sign people in with session cookies too (docs/accounts.md). `api_url`: where browsers
    /// reach this API, whose origin may send writes besides the dashboard's.
    pub fn sessions(&mut self, sessions: Arc<dyn roll_auth::Sessions>, api_url: &str) {
        self.gate = Gate::with_sessions(self.token.clone(), RULES.to_vec(), sessions, self.origins.clone(), api_url);
    }
}

impl Auth {
    /// The token and origins from the environment, refusing what isn't safe: no token while
    /// listening off loopback (or behind the edge).
    pub fn new(args: &Args) -> anyhow::Result<Self> {
        let token = match std::env::var("ROLL_ADMIN_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
            Some(t) => Some(AdminToken::new(&t).map_err(|why| anyhow::anyhow!("ROLL_ADMIN_TOKEN {why}"))?),
            None => None,
        };
        let edge = edge();
        let open: Vec<SocketAddr> = listeners(args).into_iter().filter(|a| !a.ip().is_loopback()).collect();
        let exposed = edge || !open.is_empty();
        if token.is_none() && exposed {
            let why = if edge { "ROLL_EDGE is on".to_string() } else { format!("rolld listens on {}", open.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")) };
            anyhow::bail!("{why}: set ROLL_ADMIN_TOKEN (at least {} characters) to protect the control-plane API, or listen on 127.0.0.1", AdminToken::MIN_LEN);
        }
        let origins = match std::env::var("ROLL_DASHBOARD_ORIGIN").ok().filter(|o| !o.trim().is_empty()) {
            Some(list) => Origins::parse(&list).map_err(|e| anyhow::anyhow!("ROLL_DASHBOARD_ORIGIN: {e}"))?,
            None if exposed => {
                warn!("ROLL_DASHBOARD_ORIGIN isn't set: no browser origin may call the control-plane API");
                Origins::List(Vec::new())
            }
            // With a token, sessions are on and CORS carries credentials: any page on any local
            // port reading the session (and its CSRF token) would act as whoever is signed in.
            // Only the dashboard's own origin (`ROLL_WEB_URL`) then.
            None if token.is_some() => {
                let web = args.web_url.trim();
                let origin = web.find("://").map_or(web, |i| web[i + 3..].find('/').map_or(web, |j| &web[..i + 3 + j]));
                Origins::parse(origin).map_err(|e| anyhow::anyhow!("ROLL_WEB_URL as the dashboard's origin: {e}"))?
            }
            None => Origins::Local,
        };
        match &token {
            Some(_) => info!("control-plane API: ROLL_ADMIN_TOKEN required (see crates/rolld/src/auth.rs for the routes that aren't the operator's)"),
            None => info!("control-plane API: open, no token needed (loopback only; set ROLL_ADMIN_TOKEN to require one)"),
        }
        match &origins {
            Origins::Local => info!("CORS: the dashboard on {LOCAL_HOSTS}, any port"),
            Origins::List(l) => info!(origins = ?l, "CORS: the dashboard's origins"),
        }
        let gate = Gate::new(token.clone(), RULES.to_vec());
        Ok(Self { token, gate, origins })
    }

    /// Whether requests need the token: products with credentials of their own then require
    /// them too (tasks, the streams data plane).
    pub fn enforcing(&self) -> bool {
        self.gate.enforcing()
    }

    pub fn token(&self) -> Option<AdminToken> {
        self.token.clone()
    }

    /// `router` behind the gate (`host`: where it's served, `api` or an edge host). Logs every
    /// route with its access at debug (`RUST_LOG=rolld::auth=debug`).
    pub fn gate(&self, host: &str, router: axum::Router) -> axum::Router {
        let routes = route_patterns(&router);
        let (mut public, mut product, mut credential, mut user, mut operator) = (0, 0, 0, 0, 0);
        for r in &routes {
            let access = self.gate.access(r);
            match access {
                Access::Public(_) => public += 1,
                Access::Product(_) => product += 1,
                Access::Credential(_) => credential += 1,
                Access::User(_) => user += 1,
                Access::Operator => operator += 1,
            }
            let scoped = roll_access::catalog::classified(r).unwrap_or("none");
            debug!(target: "rolld::auth", host = %host, route = %r, scoped = %scoped, access = %access, "route");
        }
        info!(host = %host, routes = routes.len(), operator, operator_or_credential = credential, user, product, public, "control-plane routes");
        router.layer(axum::middleware::from_fn_with_state(self.gate.clone(), roll_auth::check))
    }

    /// Scoped tokens' evaluator (`access.rs`): set before serving.
    pub fn set_authorizer(&self, a: std::sync::Arc<dyn roll_auth::Authorizer>) {
        self.gate.set_authorizer(a);
    }

    /// `router` behind the gate, without listing its routes (the MCP server's view of the API).
    pub fn layer(&self, router: axum::Router) -> axum::Router {
        router.layer(axum::middleware::from_fn_with_state(self.gate.clone(), roll_auth::check))
    }

    /// `router` answering the dashboard's origins (CORS), preflights included.
    pub fn cors(&self, router: axum::Router) -> axum::Router {
        router.layer(roll_auth::cors(&self.origins))
    }
}

/// Whether rolld serves the internet through its edge (`ROLL_EDGE=1`): the public listeners reach
/// products whatever addresses those bind.
pub fn edge() -> bool {
    std::env::var("ROLL_EDGE").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Every address rolld may listen on with these flags.
fn listeners(a: &Args) -> Vec<SocketAddr> {
    let (q, n) = (&a.queues, &a.nosql);
    let mut l = vec![a.api, a.proxy, a.redis, a.redis_http, a.kafka, a.s3, a.storage_api];
    l.extend([q.amqp, q.management, q.stomp, q.web_stomp, q.mqtt, q.web_mqtt, q.stream, q.http]);
    l.extend([n.nosql_cql, n.nosql_cql_shard_aware, n.nosql_alternator, n.nosql_rest]);
    l.extend([n.nosql_alternator_https, n.nosql_cql_tls, n.nosql_cql_shard_aware_tls, n.nosql_metrics].into_iter().flatten());
    l
}

/// The route patterns of `router` (axum has no API for them; its `Debug` lists them).
pub fn route_patterns(router: &axum::Router) -> Vec<String> {
    let text = format!("{router:?}");
    let mut out = Vec::new();
    // The routes, not the fallback's (`fallback_router` comes after `path_router`).
    let mut rest = text.split("fallback_router:").next().unwrap_or_default();
    // The path map's entries: `RouteId(7): "/v1/projects/{id}"`.
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let Some(after) = rest.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix("): \"") else { continue };
        let Some(end) = after.find('"') else { break };
        let path = &after[..end];
        if path.starts_with('/') && !path.contains("__private__") && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out.sort();
    out
}
