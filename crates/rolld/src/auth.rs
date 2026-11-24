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
//!   the token does; anyone else's opens roll code, the agents, their own projects and their own
//!   search clusters, and gets 403 on the rest.

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
    Rule { route: "/.well-known", access: Access::Public("OAuth discovery for MCP clients, and the AI Catalog naming MCP's server card") },
    Rule { route: "/v1/access/oauth/register", access: Access::Public("OAuth dynamic client registration (public clients, PKCE)") },
    Rule { route: "/v1/access/oauth/token", access: Access::Public("OAuth token endpoint: codes and refresh tokens are the credentials") },
    Rule { route: "/v1/access/oauth/revoke", access: Access::Public("OAuth token revocation") },
    Rule { route: "/v1/access/oauth/device_authorization", access: Access::Public("OAuth device authorization (RFC 8628): a device asks for the code its person enters") },
    Rule { route: "/v1/access/leaks/github", access: Access::Public("GitHub's secret scanning reports: GitHub's signature is the credential") },
    // roll's MCP server answers 401 itself, naming the OAuth metadata; grants are decided by the
    // evaluator before it, and each tool call goes through the gate again.
    Rule { route: "/v1/mcp", access: Access::Product("MCP: 401 with OAuth discovery without a token") },
    Rule { route: "/v1/mcp/server-card", access: Access::Public("MCP's server card: who roll's MCP server is and where to connect, before connecting") },
    // Scoped tokens: the operator's, and each signed-in user's own (the routes check which).
    Rule { route: "/v1/access", access: Access::User("access: a user's own tokens, approvals and audit") },
    // …except where approvals are posted and where the audit log streams: the operator's.
    Rule { route: "/v1/access/notify", access: Access::Operator },
    Rule { route: "/v1/access/audit/sinks", access: Access::Operator },
    // Which identity providers roll trusts, and the apps they may sign people in to.
    Rule { route: "/v1/access/enterprise", access: Access::Operator },
    // Health: load balancers and the dashboard's status; nothing a tenant owns.
    Rule { route: "/v1/health", access: Access::Public("health: Postgres control plane version and status") },
    Rule { route: "/v1/code/health", access: Access::Public("health: load balancers") },
    Rule { route: "/v1/tasks/health", access: Access::Public("health: tasks node status") },
    Rule { route: "/v1/queues/health", access: Access::Public("health: queues node status") },
    Rule { route: "/healthz", access: Access::Public("health: load balancers") },
    // Version, node and which products are on: the dashboard's sidebar reads it before sign-in.
    // It (and the 404s of products that are off) is merged in front of the gate in main.rs.
    Rule { route: "/v1/info", access: Access::Public("version, node and products on, for the dashboard before sign-in") },
    Rule { route: "/v1/previews/hooks", access: Access::Public("previews' deploy hooks: the secret in the URL (`rlb_`, a hash kept); anything but a live hook is a 404") },
    Rule { route: "/v1/previews/{id}/oidc", access: Access::Public("roll's word for a preview's own service (its OIDC token): the address is the service's own (derived per preview and service, masked in what it shows); anything but a live trusted preview's is a 404 or 403") },
    Rule { route: "/v1/previews/{id}/otlp", access: Access::Public("a preview's OpenTelemetry intake: its services' SDKs post spans with the token in the path (derived per preview, masked in what it shows); anything but a live preview's token is a 404") },
    Rule { route: "/v1/previews/{id}/screenshot.png", access: Access::Public("a public preview's screenshot behind a signed, expiring link (GitHub fetches the pull request comment's image anonymously)") },
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
    Rule { route: "/v1/previews", access: Access::User("previews: the projects a person is a member of, each route as their role there allows (docs/members.md)") },
    // roll search's control plane: the operator and every signed-in person, each seeing their own
    // clusters (search scopes by SignedIn; another's answers 404). Clusters' data plane is the
    // REST port, with the cluster's own credentials.
    Rule { route: "/v1/search", access: Access::User("search clusters: a user's own") },
    // Node-to-node hops are signed with the cluster secret; computes fetch WAL with their token.
    Rule { route: "/internal", access: Access::Product("internal: signed hops and the WAL token") },
    // Storage: IAM users signed in to the dashboard (console sessions) act as themselves.
    Rule { route: "/v1/storage/console/signin", access: Access::Public("storage sign-in: checks the IAM user's password and MFA") },
    Rule { route: "/v1/storage/console/session", access: Access::Product("storage: who a console session is, for that session's token only") },
    Rule { route: "/v1/storage", access: Access::Credential("a storage console session") },
    // roll secrets: the operator, a signed-in person on their own projects, or a roll token with
    // a rule on a project's environments and folders, checked on every request (docs/secrets.md,
    // "Access").
    Rule { route: "/v1/secrets", access: Access::UserOrCredential("a roll token (rlt_…), decided by roll-access") },
    // The deployment key's state is about the whole server: never a person's session.
    Rule { route: "/v1/secrets/health", access: Access::Credential("a roll token (rlt_…); the operator's answer") },
    // roll's issuer key signs for every project: rotating it is the operator's.
    Rule { route: "/v1/secrets/issuer", access: Access::Operator },
    // So does the audit log's digest key, and its public keys are what exports are checked with.
    Rule { route: "/v1/secrets/audit/key", access: Access::Operator },
    Rule { route: "/v1/secrets/audit/jwks", access: Access::Public("the audit log's digest keys: public, as an export is checked against them offline") },
    // A gateway's link (docs/secrets.md, "Gateways"): only its own token or a workload identity's
    // login opens it, never a person's session (a WebSocket upgrade is a GET with no CSRF check).
    Rule { route: "/v1/secrets/gateways/{id}/connect", access: Access::Credential("a gateway's own roll token (rlt_…) or a workload identity's login; the operator's token is refused (403)") },
    // A one-time share's link: the value is encrypted with a key only the link holds, and opens
    // a few times at most (docs/secrets.md, "Sharing a value once").
    // A sealed node's unseal (docs/secrets.md, "Starting sealed"): nothing exists to authenticate
    // against until the key does; the key's shares are the credential, limited per address and
    // per split. An unsealed node answers that it isn't sealed.
    Rule { route: "/v1/secrets/unseal", access: Access::Public("unsealing a sealed node: the deployment key's shares are the credential") },
    Rule { route: "/v1/secrets/shares", access: Access::Public("a one-time share's link: ciphertext only, the key stays in the link") },
    // A secret request's link: for someone outside roll, who has no roll credential. It shows
    // only the note and key names, takes values sealed to the request's own key, is answered a
    // set number of times before it expires, and lands as whoever asked could write
    // (docs/secrets.md, "Asking for a value").
    Rule { route: "/v1/secrets/value-requests", access: Access::Public("a secret request's link: someone outside roll sends sealed values in, as whoever asked") },
    Rule { route: "/v1/secrets/ocsp", access: Access::Public("a project CA's OCSP responder, which anyone checking its certificates asks") },
    Rule { route: "/v1/secrets/crl", access: Access::Public("a project CA's certificate revocation list, which anyone checking its certificates fetches") },
    // ACME (RFC 8555) on a project's CA: the account key's JWS, made with an External Account
    // Binding an admin of the project issued, and looked up on every request.
    Rule { route: "/v1/secrets/acme", access: Access::Public("ACME on a project's CA: the account key's signature and its External Account Binding are the credential") },
    Rule { route: "/v1/secrets/login", access: Access::Public("a workload's OIDC token is the credential (workload identities)") },
    Rule { route: "/v1/secrets/oidc", access: Access::Public("roll's OIDC issuer: its discovery document and public keys, which GCP and Azure fetch anonymously") },
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
        let methods = route_methods(&router);
        let (mut public, mut product, mut credential, mut user, mut operator) = (0, 0, 0, 0, 0);
        for r in &routes {
            let access = self.gate.access(r);
            match access {
                Access::Public(_) => public += 1,
                Access::Product(_) => product += 1,
                Access::Credential(_) => credential += 1,
                Access::User(_) | Access::UserOrCredential(_) => user += 1,
                Access::Operator => operator += 1,
            }
            let scoped = roll_access::catalog::classified(r).unwrap_or("none");
            let m = methods.iter().find(|(p, _)| p == r).map_or("*", |(_, m)| m.as_str());
            debug!(target: "rolld::auth", host = %host, route = %r, scoped = %scoped, access = %access, methods = %m, "route");
        }
        info!(host = %host, routes = routes.len(), operator, operator_or_credential = credential, user, product, public, "control-plane routes");
        router.layer(axum::middleware::from_fn_with_state(self.gate.clone(), roll_auth::check))
    }

    /// Scoped tokens' evaluator (`access.rs`): set before serving.
    /// How the gate reads a client's address (roll-access's, with its trusted proxies).
    pub fn set_client_ip(&self, f: roll_auth::ClientIpFn) {
        self.gate.set_client_ip(f);
    }

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
    l.push(a.search.search_http);
    l
}

/// The methods each route of `router` serves, from its `Debug` (`allow_header: Bytes(b"GET,HEAD")`):
/// `(pattern, "GET,POST")`, without `HEAD` (GET's); `*` where it can't tell (a nested service).
pub fn route_methods(router: &axum::Router) -> Vec<(String, String)> {
    let text = format!("{router:?}");
    let routes = text.split("fallback_router:").next().unwrap_or_default();
    let (routes, paths) = routes.split_once("node: Node { paths:").unwrap_or((routes, ""));
    let mut allow: Vec<(String, String)> = Vec::new();
    let mut rest = routes;
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        // This route's entry runs to the next `RouteId(`.
        let entry = rest.find("RouteId(").map_or(rest, |j| &rest[..j]);
        let methods = entry
            .split_once("allow_header: Bytes(b\"")
            .and_then(|(_, a)| a.split_once('"'))
            .map(|(a, _)| a.split(',').filter(|m| *m != "HEAD" && !m.is_empty()).collect::<Vec<_>>().join(","))
            .unwrap_or_else(|| "*".into());
        allow.push((id, methods));
    }
    let mut out = Vec::new();
    let mut rest = paths;
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let Some(after) = rest.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix("): \"") else { continue };
        let Some(end) = after.find('"') else { break };
        let path = &after[..end];
        if path.starts_with('/') && !path.contains("__private__") {
            let m = allow.iter().find(|(a, _)| *a == id).map_or("*".to_string(), |(_, m)| if m.is_empty() { "*".into() } else { m.clone() });
            out.push((path.to_string(), m));
        }
    }
    out
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

/// A hop another node signed for roll's own work for a grant (`roll_cluster::Forwarded::acting`:
/// an MCP Events delivery reading a feed whose route runs on its owner here) is that work here too:
/// the gate acts for the grant, looked up again as it is now. Only a verified hop carries it
/// (`accept_forwarded` refuses an unsigned `x-roll-code-*` header), and never beside a token.
pub async fn act_as_from_hop(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if !req.headers().contains_key(axum::http::header::AUTHORIZATION)
        && let Some(grant) = req.extensions().get::<roll_cluster::Forwarded>().and_then(|f| f.acting.clone())
    {
        req.extensions_mut().insert(roll_auth::ActAs(grant.into()));
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use roll_auth::Access;

    /// What a person's own token can reach (`roll_access::USER_REACH`: the tools their MCP client is
    /// shown, the token builder's products) is what the gate lets their session call: every
    /// `/v1/<product>` a rule serves to people, plus roll code and agents (their own owners). A
    /// product that becomes a member product (previews in #524) can't be left out of either.
    #[test]
    fn a_persons_reach_is_what_the_gate_serves_people() {
        let served: Vec<&str> = super::RULES
            .iter()
            .filter(|r| matches!(r.access, Access::User(_) | Access::UserOrCredential(_)))
            .filter_map(|r| r.route.strip_prefix("/v1/").filter(|p| !p.contains('/')))
            .filter(|p| *p != "access")
            .collect();
        let mut want: Vec<&str> = served.iter().copied().chain(["code", "agents"]).collect();
        let mut reach = roll_access::USER_REACH.to_vec();
        want.sort_unstable();
        reach.sort_unstable();
        assert_eq!(reach, want, "roll_access::USER_REACH against the gate's rules for people");
        for p in roll_auth::roles::MEMBER_PRODUCTS {
            assert!(roll_access::USER_REACH.contains(p), "member product {p} isn't in USER_REACH");
        }
    }
}
