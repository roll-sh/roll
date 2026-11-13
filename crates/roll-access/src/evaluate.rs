//! The one evaluator (docs/permissions.md, "Evaluation"). rolld's gate hands it every request
//! that bears a scoped token or an OAuth access token, on every route; it answers it or sends it
//! on as the operator's, with the [`roll_auth::Grant`] in its extensions.

use crate::catalog::{self, EnvFrom, Row, Tgt};
use futures::StreamExt;
use crate::store::{Ask, AuditEvent, Grant, REFRESH_PREFIX};
use crate::{Access, Error, hash, new_id, redact};
use axum::body::Body;
use axum::extract::{MatchedPath, RawPathParams, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use roll_auth::scope::{Action, Outcome, Resource, Resources, Scope, Target};
use serde_json::{Value, json};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

/// Bodies read to decide or ask about a request, at most.
const MAX_BODY: usize = 4 << 20;
/// Answers read to filter or redact, at most.
const MAX_ANSWER: usize = 64 << 20;
/// Names a request's approval when it's sent again.
pub const APPROVAL_HEADER: &str = "x-roll-approval";
/// Names the workspace project a create goes in.
pub const PROJECT_HEADER: &str = "x-roll-project";

/// In the extensions of a request the MCP server makes for a tool call: the tool, its redacted
/// arguments and the client's address (an in-process request has no peer of its own).
#[derive(Clone, Debug)]
pub struct McpCall {
    pub tool: Arc<str>,
    pub args: Arc<Value>,
    pub ip: Option<IpAddr>,
}

/// The evaluator, as rolld's gate wants it.
pub struct Evaluator(pub Arc<Access>);

impl roll_auth::Authorizer for Evaluator {
    fn claims(&self, token: &str) -> bool {
        roll_auth::is_grant_token(token) || token.starts_with(REFRESH_PREFIX)
    }

    fn handle(&self, req: Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send + 'static>> {
        Box::pin(decide(self.0.clone(), req, next))
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// The client's address: the MCP call's, the one the first node saw (a signed hop), the forwarded
/// one behind the edge, or the peer.
pub fn client_ip(req: &Request, trust_forwarded_for: bool) -> Option<IpAddr> {
    if let Some(c) = req.extensions().get::<McpCall>() {
        return c.ip;
    }
    if let Some(roll_auth::ClientIp(ip)) = req.extensions().get::<roll_auth::ClientIp>() {
        return Some(*ip);
    }
    if let Some(f) = req.extensions().get::<roll_cluster::Forwarded>() {
        return f.client.as_deref().and_then(|c| c.parse().ok());
    }
    if trust_forwarded_for
        && let Some(ip) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).and_then(|v| v.trim().parse().ok())
    {
        return Some(ip);
    }
    req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip())
}

fn deny(status: StatusCode, code: &str, message: String, extra: Value) -> Response {
    let mut body = json!({ "error": message, "code": code });
    if let (Some(b), Value::Object(e)) = (body.as_object_mut(), extra) {
        b.extend(e);
    }
    (status, axum::Json(body)).into_response()
}

fn invalid_token(access: &Access) -> Response {
    let mut r = deny(StatusCode::UNAUTHORIZED, "invalid_token", "the token isn't valid: it was revoked, expired, or never existed".into(), json!({}));
    let meta = format!("Bearer error=\"invalid_token\", resource_metadata=\"{}/.well-known/oauth-protected-resource/v1/mcp\"", access.config.api_url.trim_end_matches('/'));
    if let Ok(v) = HeaderValue::from_str(&meta) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

/// What a request is about, from its row and path.
pub(crate) fn target(access: &Access, row: &Row, params: &[(String, String)], query: Option<&str>, headers: &axum::http::HeaderMap) -> Target {
    let param = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let product = row.product.to_string();
    // Storage acts in the account a request names. Bucket names repeat across accounts, so a
    // bucket there is `<account>/<bucket>` (a rule names it so); anything else in another account
    // (lists, keys, roles) is about the product as a whole.
    if let Some(account) = storage_account(row, query, headers) {
        return match row.target {
            Tgt::Param(names, kind) => {
                let bucket = names.iter().filter_map(|n| param(n)).collect::<Vec<_>>().join("/");
                Target::One(access.resource(row.product, kind, &format!("{account}/{bucket}")))
            }
            // Its buckets, filtered to the ones the token has there.
            Tgt::List { .. } => Target::List { product },
            _ => Target::Product { product },
        };
    }
    match row.target {
        Tgt::Param(names, kind) => {
            let id = names.iter().filter_map(|n| param(n)).collect::<Vec<_>>().join("/");
            let mut r = access.resource(row.product, kind, &id);
            match row.env {
                EnvFrom::Param(p) => r.environment = param(p).or(r.environment),
                EnvFrom::Fixed(e) => r.environment = Some(e.into()),
                EnvFrom::None => {}
            }
            if let Some(name) = row.path_query {
                let path = query.and_then(|qs| url::form_urlencoded::parse(qs.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned()));
                r.path = Some(path.filter(|p| !p.is_empty()).unwrap_or_else(|| "/".into()));
            }
            Target::One(r)
        }
        Tgt::List { .. } => Target::List { product },
        Tgt::Create { project, .. } => {
            let project = project.and_then(param).or_else(|| headers.get(PROJECT_HEADER).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string()));
            let environment = match row.env {
                EnvFrom::Param(p) => param(p),
                EnvFrom::Fixed(e) => Some(e.into()),
                EnvFrom::None => None,
            };
            Target::Create { product, project, environment }
        }
        Tgt::Product | Tgt::AnyGrant => Target::Product { product },
    }
}

/// The storage account a request names (`X-Roll-Account`, else `?account=`), if any.
fn storage_account(row: &Row, query: Option<&str>, headers: &axum::http::HeaderMap) -> Option<String> {
    if row.product != "storage" {
        return None;
    }
    let from_query = || query.and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "account").map(|(_, v)| v.into_owned()));
    headers.get("x-roll-account").and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string()).or_else(from_query).filter(|a| !a.is_empty())
}

pub(crate) fn describe(t: &Target) -> Option<String> {
    match t {
        Target::One(r) => r.id.clone(),
        Target::Create { project: Some(p), .. } => Some(format!("new in {p}")),
        _ => None,
    }
}

/// Whether a list needs filtering: some rule about the product names resources, environments or
/// paths (a rule for every resource needs none).
fn narrowed(scope: &Scope, product: &str, action: Action) -> bool {
    // A list answers at once: any rule about it that's narrower than every resource, or that
    // asks, means each item is checked, and only the ones allowed outright stay.
    let wide_allow = |r: &roll_auth::scope::Rule| {
        r.resources == Resources::All && r.environments.is_none() && r.paths.is_none() && r.actions.get(&action) == Some(&roll_auth::scope::Effect::Allow)
    };
    let matching: Vec<_> = scope.0.iter().filter(|r| (r.product == "*" || r.product == product) && r.actions.contains_key(&action)).collect();
    !matching.iter().any(|r| wide_allow(r))
}

struct Ctx {
    access: Arc<Access>,
    grant: Grant,
    started: Instant,
    ip: Option<IpAddr>,
    mcp: Option<McpCall>,
    method: String,
    route: String,
    /// What a request that isn't an MCP call did, for the log (a push's ref updates).
    args: Option<Value>,
    /// A list's items are named under this (a named storage account's buckets: `<account>/<name>`).
    list_prefix: Option<String>,
}

impl Ctx {
    fn audit(&self, row: Option<&Row>, action: Option<Action>, resource: Option<String>, decision: &str, approval: Option<&str>, status: StatusCode) {
        let e = AuditEvent {
            id: new_id("aud"),
            at: code_sql::now_ms(),
            grant_id: Some(self.grant.id.clone()),
            grant_name: Some(self.grant.name.clone()),
            source: if self.mcp.is_some() { "mcp" } else { "api" }.into(),
            tool: self.mcp.as_ref().map(|m| m.tool.to_string()),
            method: self.method.clone(),
            route: self.route.clone(),
            product: row.map(|r| r.product.to_string()),
            action: action.map(|a| a.as_str().to_string()),
            resource,
            decision: decision.into(),
            approval: approval.map(Into::into),
            status: status.as_u16(),
            args: self.mcp.as_ref().map(|m| (*m.args).clone()).or_else(|| self.args.clone()),
            ip: self.ip.map(|i| i.to_string()),
            duration_ms: self.started.elapsed().as_millis() as i64,
        };
        let access = self.access.clone();
        tokio::task::spawn_blocking(move || access.store.audit(&e));
    }
}

async fn decide(access: Arc<Access>, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let Some(token) = roll_auth::grant_credential(req.headers()) else { return invalid_token(&access) };
    if token.starts_with(REFRESH_PREFIX) {
        return invalid_token(&access);
    }
    let lookup = {
        let a = access.clone();
        blocking(move || a.store.by_secret(&token)).await
    };
    let grant = match lookup {
        Ok(Some(g)) => g,
        Ok(None) => return invalid_token(&access),
        Err(e) => return e.into_response(),
    };
    let ip = client_ip(&req, access.config.trust_forwarded_for);
    let route = req.extensions().get::<MatchedPath>().map_or_else(|| "unmatched".to_string(), |m| m.as_str().to_string());
    // Forwarded by a node that decided it for this grant already (a signed hop to an owner): a
    // person isn't asked twice, and the first node logs it. Everything else is decided again.
    let decided_before = req.extensions().get::<roll_cluster::Forwarded>().and_then(|f| f.decided.as_deref()) == Some(grant.id.as_str());
    // A grant's request limit: an MCP message counts once (not the route its tool calls), a hop
    // counts where it was decided. Over it, nothing is written: a flood stays cheap.
    let mut limit = None;
    if let Some(rate) = grant.rate_per_minute.filter(|_| !decided_before && req.extensions().get::<McpCall>().is_none()) {
        match access.limiter.check(&grant.id, rate, Instant::now()) {
            crate::rate::Verdict::Over { retry_after } => return rate_limited(rate, retry_after),
            crate::rate::Verdict::Allowed { remaining, reset } => limit = Some((rate, remaining, reset)),
        }
    }
    let mut ctx = Ctx { access: access.clone(), grant, started, ip, mcp: req.extensions().get::<McpCall>().cloned(), method: req.method().as_str().to_string(), route, args: None, list_prefix: None };
    if !decided_before {
        let (a, id, ip) = (access.clone(), ctx.grant.id.clone(), ip.map(|i| i.to_string()));
        tokio::task::spawn_blocking(move || a.store.touch(&id, ip.as_deref()));
    }
    if !ctx.grant.ip_allow.is_empty() {
        let nets = crate::store::parse_ip_allow(&ctx.grant.ip_allow).unwrap_or_default();
        if !ip.is_some_and(|ip| nets.iter().any(|n| n.contains(&ip))) {
            ctx.audit(None, None, None, "deny", None, StatusCode::FORBIDDEN);
            let shown = ip.map_or_else(|| "an unknown address".to_string(), |i| i.to_string());
            return deny(StatusCode::FORBIDDEN, "ip_not_allowed", format!("this token can't be used from {shown}"), json!({}));
        }
    }
    // A user's own token acts as them (docs/permissions.md, "Users' own tokens"): as they are now
    // (disabled or gone, the token is dead), and only where their session reaches.
    let person = match ctx.grant.user.clone() {
        None => None,
        Some(user) => {
            let Some(people) = access.people.get().cloned() else { return invalid_token(&access) };
            match people.person(&user).await {
                Ok(Some(p)) => Some(p),
                Ok(None) => return invalid_token(&access),
                Err(e) => {
                    tracing::warn!(error = %e, "looking up a token's user");
                    return deny(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "accounts are unavailable; try again".into(), json!({}));
                }
            }
        }
    };
    let as_user = person.as_ref().is_some_and(|p| !p.admin);
    let reachable = matches!(
        req.extensions().get::<roll_auth::RouteAccess>(),
        Some(roll_auth::RouteAccess(roll_auth::Access::Public(_) | roll_auth::Access::Product(_) | roll_auth::Access::User(_)))
    );
    if as_user && !reachable {
        ctx.audit(None, None, None, "deny", None, StatusCode::FORBIDDEN);
        return deny(StatusCode::FORBIDDEN, "operator_only", "a user's token reaches what their account does: roll code, agents and projects".into(), json!({}));
    }
    // A nested service (storage) sees its path without the prefix: the original has it.
    let mut original = req.extensions().get::<axum::extract::OriginalUri>().map_or_else(|| req.uri().clone(), |o| o.0.clone());
    // A signed hop to an owner or leader: the request it carries is the one to decide.
    if let Some((_, h)) = catalog::FORWARDS.iter().find(|(r, _)| *r == ctx.route) {
        match req.headers().get(*h).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<axum::http::Uri>().ok()) {
            Some(u) => original = u,
            None => return deny(StatusCode::BAD_REQUEST, "bad_request", "a forwarded request without its path".into(), json!({})),
        }
    }
    let Some(found) = catalog::find(&ctx.method, &ctx.route, original.path()) else {
        ctx.audit(None, None, None, "deny", None, StatusCode::FORBIDDEN);
        return deny(StatusCode::FORBIDDEN, "operator_only", format!("{} {} is the operator's: no token can call it", ctx.method, ctx.route), json!({}));
    };
    let row = found.row;
    // Matched by its real path (a catch-all, a nest, a signed hop): the log names the real route.
    if found.params.is_some() {
        ctx.route = row.route.to_string();
    }
    let scope = Arc::new(ctx.grant.rules.clone());
    let (mut parts, body) = req.into_parts();
    let params: Vec<(String, String)> = match found.params {
        Some(p) => p,
        None => {
            use axum::extract::FromRequestParts;
            match RawPathParams::from_request_parts(&mut parts, &()).await {
                Ok(p) => p.iter().map(|(k, v)| (k.to_string(), percent_decode(v))).collect(),
                Err(_) => Vec::new(),
            }
        }
    };
    let target = {
        let (a, q, h) = (access.clone(), original.query().map(str::to_string), parts.headers.clone());
        let params = params.clone();
        match tokio::task::spawn_blocking(move || target(&a, row, &params, q.as_deref(), &h)).await {
            Ok(t) => t,
            Err(e) => return Error::Internal(e.to_string()).into_response(),
        }
    };
    ctx.list_prefix = storage_account(row, original.query(), &parts.headers);
    // The body, when the action depends on it.
    let mut buffered: Option<bytes::Bytes> = None;
    let mut body = Some(body);
    if row.needs_body() {
        match axum::body::to_bytes(body.take().unwrap_or_default(), MAX_BODY).await {
            Ok(b) => buffered = Some(b),
            Err(_) => return deny(StatusCode::PAYLOAD_TOO_LARGE, "too_large", format!("the body is over {} MiB", MAX_BODY >> 20), json!({})),
        }
    }
    let json_body = buffered.as_ref().and_then(|b| serde_json::from_slice::<Value>(b).ok());
    // What the request needs: one action, or every action a push's ref updates take.
    let mut needs = vec![row.action(&ctx.method, &params, json_body.as_ref())];
    // A git push: its ref updates come first; the pack after them streams on untouched.
    let mut git_commands: Option<bytes::Bytes> = None;
    if matches!(row.act, catalog::Act::GitPush) {
        needs = catalog::UNREAD_PUSH.to_vec();
        // A compressed body isn't read: it needs everything a push can do.
        let encoded = parts.headers.get(axum::http::header::CONTENT_ENCODING).is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"));
        if !encoded {
            let (prefix, rest) = peek_push(body.take().unwrap_or_default()).await;
            let repo = describe(&target).unwrap_or_default();
            let a2 = access.clone();
            let default = tokio::task::spawn_blocking(move || a2.resolvers.resolve("default-branch", &repo).map(|r| r.id)).await.ok().flatten();
            if let catalog::Push::Commands(updates) = catalog::git_push(&prefix, default.as_deref()) {
                needs = updates.iter().map(|u| u.action).collect();
                ctx.args = Some(json!({ "updates": updates }));
            }
            let head = prefix.clone();
            body = Some(Body::from_stream(futures::stream::once(async move { Ok::<_, axum::Error>(head) }).chain(rest)));
            git_commands = Some(prefix);
        }
    }
    let (mut outcome, action) = if matches!(row.target, Tgt::AnyGrant) { (Outcome::Allow, needs[0]) } else { weakest(&needs, |a| scope.outcome(&target, a)) };
    // Putting a resource in a workspace project (or taking it out) changes which tokens reach it:
    // it also needs `admin` on that resource, where it is now, allowed outright.
    if let Some((product, r#ref)) = catalog::placement(row, &params) {
        let (a, product2, r2) = (access.clone(), product, r#ref.clone());
        let moved = tokio::task::spawn_blocking(move || a.resource(product2, product2, &r2)).await.ok();
        let ok = moved.is_some_and(|m| scope.outcome(&Target::One(m), Action::Admin) == Outcome::Allow);
        if !ok {
            outcome = Outcome::Deny;
        }
    }
    let resource = describe(&target);
    let mut approval_used: Option<String> = None;
    match outcome {
        Outcome::Deny => {
            ctx.audit(Some(row), Some(action), resource.clone(), "deny", None, StatusCode::FORBIDDEN);
            let what = resource.as_deref().map_or_else(|| row.product.to_string(), |r| format!("{} {r}", row.product));
            return deny(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!("this token can't {action} {what}"),
                json!({ "needs": { "product": row.product, "action": action, "resource": resource } }),
            );
        }
        Outcome::Ask if decided_before => {}
        Outcome::Ask => {
            // A push is bound to its ref updates (the pack isn't read to ask about it).
            if let Some(c) = &git_commands {
                buffered = Some(c.clone());
            }
            if buffered.is_none() {
                match axum::body::to_bytes(body.take().unwrap_or_default(), MAX_BODY).await {
                    Ok(b) => buffered = Some(b),
                    Err(_) => return deny(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "the body is too large to ask about".into(), json!({})),
                }
            }
            let bytes = buffered.clone().unwrap_or_default();
            // Bound to everything that decides what the request does: the method, path and body,
            // what it's about, and the headers that change it (account, project, reviewer).
            let bound: Vec<String> = ["x-roll-account", "x-roll-project", "x-roll-user", "x-roll-agent"]
                .iter()
                .map(|h| format!("{h}={}", parts.headers.get(*h).and_then(|v| v.to_str().ok()).unwrap_or("")))
                .collect();
            let request_hash = hash(&format!(
                "{}\n{}\n{}\n{:?}\n{}",
                ctx.method,
                original.path_and_query().map_or("", |p| p.as_str()),
                hex::encode(sha2_of(&bytes)),
                target,
                bound.join("\n")
            ));
            let asked = Asked { headers: &parts.headers, path: original.path(), body: &bytes, request_hash, args: ctx.args.clone() };
            match ask(&ctx, row, action, resource.clone(), asked).await {
                Ok(id) => approval_used = Some(id),
                Err(resp) => return *resp,
            }
        }
        Outcome::Allow => {}
    }
    // Allowed: on as the operator's, narrowed, with the grant for products and the audit. A grant
    // reviews and comments only as itself (roll code takes these headers from the operator).
    parts.headers.remove("x-roll-user");
    if let Ok(v) = axum::http::HeaderValue::from_str(&reviewer_name(&ctx.grant)) {
        parts.headers.insert("x-roll-agent", v);
    }
    // As the operator, narrowed; a user's token as that user (an admin also as the operator).
    if !as_user {
        parts.extensions.insert(roll_auth::Operator);
    }
    if let Some(p) = &person {
        parts.extensions.insert(roll_auth::SignedIn { session: ctx.grant.id.clone(), ..p.clone() });
    }
    if let Some(ip) = ctx.ip {
        parts.extensions.insert(roll_auth::ClientIp(ip));
        parts.extensions.insert(roll_cluster::ClientAddr(ip.to_string()));
    }
    // Forwarded to an owner, it carries this decision (signed) so a person isn't asked again.
    parts.extensions.insert(roll_cluster::Decided(ctx.grant.id.clone()));
    parts.extensions.insert(roll_auth::Grant {
        id: ctx.grant.id.as_str().into(),
        name: ctx.grant.name.as_str().into(),
        kind: ctx.grant.kind(),
        scope: scope.clone(),
        user: ctx.grant.user.as_deref().map(Into::into),
    });
    // A list filtered, an answer's credentials redacted, a create placed: the answer is read.
    let filter_list = matches!(row.target, Tgt::List { .. }) && narrowed(&scope, row.product, action);
    let redact_fields = !row.reveals.is_empty() && scope.outcome(&target, Action::Reveal) != Outcome::Allow;
    let place = match (&row.target, &target) {
        (Tgt::Create { place: Some(kind), id_field, .. }, Target::Create { project: Some(p), .. }) => Some((*kind, *id_field, p.clone())),
        _ => None,
    };
    let body = match (buffered, git_commands.is_some()) {
        (Some(b), false) => Body::from(b),
        _ => body.take().unwrap_or_default(),
    };
    let resp = next.run(Request::from_parts(parts, body)).await;
    let status = resp.status();
    let resp = if status.is_success() && (filter_list || redact_fields || place.is_some()) {
        post_process(&ctx, row, action, &scope, resp, filter_list, redact_fields, place).await
    } else {
        resp
    };
    let mut resp = resp;
    if let Some((rate, remaining, reset)) = limit {
        limit_headers(resp.headers_mut(), rate, remaining, reset);
    }
    if !decided_before {
        ctx.audit(Some(row), Some(action), resource, if approval_used.is_some() { "ask" } else { "allow" }, approval_used.as_deref(), resp.status());
    }
    resp
}

/// Over a grant's limit: 429, when to come back (`Retry-After`, whole seconds), and the limit
/// (`RateLimit-*`, the IETF draft's fields, as Stripe and GitHub send theirs).
fn rate_limited(rate: u32, retry_after: std::time::Duration) -> Response {
    let secs = retry_after.as_secs_f64().ceil().max(1.0) as u64;
    let mut r = deny(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("this token may make {rate} requests a minute: try again in {secs} s"),
        json!({ "retry_after": secs, "limit": rate }),
    );
    let h = r.headers_mut();
    h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
    limit_headers(h, rate, 0, retry_after);
    r
}

fn limit_headers(h: &mut axum::http::HeaderMap, rate: u32, remaining: u32, reset: std::time::Duration) {
    h.insert("ratelimit-limit", HeaderValue::from(rate));
    h.insert("ratelimit-remaining", HeaderValue::from(remaining));
    h.insert("ratelimit-reset", HeaderValue::from(reset.as_secs_f64().ceil() as u64));
}

/// The decision on everything a request needs: the weakest outcome, and the action it's for
/// (the strongest among equals).
fn weakest(needs: &[Action], outcome: impl Fn(Action) -> Outcome) -> (Outcome, Action) {
    let rank = |o: Outcome| match o {
        Outcome::Deny => 0,
        Outcome::Ask => 1,
        Outcome::Allow => 2,
    };
    let mut sorted = needs.to_vec();
    sorted.sort_by(|a, b| b.cmp(a));
    sorted.dedup();
    let mut out = (Outcome::Allow, sorted[0]);
    for a in sorted {
        let o = outcome(a);
        if rank(o) < rank(out.0) {
            out = (o, a);
        }
    }
    out
}

/// The name a grant reviews and comments as in roll code: `token:<its name>` (roll code's agent
/// name rules: 1-100 of `[A-Za-z0-9._@/:+-]`).
fn reviewer_name(g: &Grant) -> String {
    let name: String = g.name.chars().map(|c| if c.is_ascii_alphanumeric() || "._@/+-".contains(c) { c } else { '-' }).take(90).collect();
    format!("token:{name}")
}

/// The start of a push's body: its commands (pkt-lines to the first flush, at most 1 MiB; it
/// stops at the first line that isn't one), and the rest of the stream.
async fn peek_push(body: Body) -> (bytes::Bytes, axum::body::BodyDataStream) {
    use futures::StreamExt;
    let mut stream = body.into_data_stream();
    let mut prefix = bytes::BytesMut::new();
    while catalog::git_push(&prefix, None) == catalog::Push::More && prefix.len() < (1 << 20) {
        match stream.next().await {
            Some(Ok(chunk)) => prefix.extend_from_slice(&chunk),
            _ => break,
        }
    }
    (prefix.freeze(), stream)
}

fn sha2_of(b: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(b).into()
}

/// A path parameter, percent-decoded (`RawPathParams` keeps them as sent).
fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// The request an approval is for.
struct Asked<'a> {
    headers: &'a axum::http::HeaderMap,
    path: &'a str,
    body: &'a [u8],
    request_hash: String,
    /// What it does, when its body isn't JSON (a push's ref updates).
    args: Option<Value>,
}

/// Wait for a person: an approval sent with the request (`X-Roll-Approval`), or a new one.
async fn ask(ctx: &Ctx, row: &Row, action: Action, resource: Option<String>, req: Asked<'_>) -> Result<String, Box<Response>> {
    let Asked { headers, path, body, request_hash, args } = req;
    let access = &ctx.access;
    let existing = headers.get(APPROVAL_HEADER).and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string());
    let approval = match existing {
        Some(id) => {
            let a = access.clone();
            let id2 = id.clone();
            let found = blocking(move || a.store.approval(&id2)).await.map_err(|e| Box::new(e.into_response()))?;
            if found.grant_id != ctx.grant.id || found.request_hash != request_hash {
                return Err(Box::new(deny(StatusCode::FORBIDDEN, "approval_mismatch", format!("{id} was asked for another request"), json!({}))));
            }
            found
        }
        None => {
            let args = args.unwrap_or_else(|| redact::body(row, body));
            let summary = redact::summary(row, action, &ctx.method, resource.as_deref(), ctx.mcp.as_ref().map(|m| &*m.tool));
            let a = access.clone();
            let grant = ctx.grant.clone();
            let (product, method, route, path, tool) =
                (row.product.to_string(), ctx.method.clone(), ctx.route.clone(), path.to_string(), ctx.mcp.as_ref().map(|m| m.tool.to_string()));
            let res = resource.clone();
            blocking(move || {
                a.store.ask(Ask {
                    grant: &grant,
                    product: &product,
                    action: action.as_str(),
                    resource: res.as_deref(),
                    method: &method,
                    route: &route,
                    path: &path,
                    summary,
                    args,
                    tool: tool.as_deref(),
                    request_hash,
                })
            })
            .await
            .map_err(|e| Box::new(e.into_response()))
            .inspect(|a| access.announce(a))?
        }
    };
    let decided = wait(access, &approval.id, access.config.approval_wait).await.map_err(|e| Box::new(e.into_response()))?;
    let url = format!("{}/dashboard/access/approvals?id={}", access.config.web_url.trim_end_matches('/'), decided.id);
    match decided.state.as_str() {
        "approved" => {
            let (a, id, g, h) = (access.clone(), decided.id.clone(), ctx.grant.id.clone(), decided.request_hash.clone());
            if blocking(move || a.store.consume(&id, &g, &h)).await.map_err(|e| Box::new(e.into_response()))? {
                Ok(decided.id)
            } else {
                Err(Box::new(deny(StatusCode::FORBIDDEN, "approval_used", format!("{} was already used", decided.id), json!({ "approval": decided.id }))))
            }
        }
        "denied" => {
            ctx.audit(Some(row), Some(action), resource, "deny", Some(&decided.id), StatusCode::FORBIDDEN);
            let why = decided.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            Err(Box::new(deny(StatusCode::FORBIDDEN, "denied", format!("a person denied this request{why}"), json!({ "approval": decided.id }))))
        }
        "pending" => {
            ctx.audit(Some(row), Some(action), resource, "ask", Some(&decided.id), StatusCode::FORBIDDEN);
            Err(Box::new(deny(
                StatusCode::FORBIDDEN,
                "approval_pending",
                format!("waiting for a person to approve this request: {url}. Send it again with X-Roll-Approval: {} to wait more, or run it once approved", decided.id),
                json!({ "approval": decided.id, "url": url }),
            )))
        }
        "used" => Err(Box::new(deny(StatusCode::FORBIDDEN, "approval_used", format!("{} was already used: send the request again to ask again", decided.id), json!({ "approval": decided.id })))),
        other => Err(Box::new(deny(StatusCode::FORBIDDEN, "approval_expired", format!("the approval is {other}: send the request again to ask again"), json!({ "approval": decided.id })))),
    }
}

/// An approval once it's decided, or as it is after `max`.
pub async fn wait(access: &Arc<Access>, id: &str, max: std::time::Duration) -> crate::Result<crate::store::Approval> {
    let deadline = tokio::time::Instant::now() + max;
    let mut wake = access.store.db.listen(crate::APPROVALS_CHANNEL);
    loop {
        let (a, id2) = (access.clone(), id.to_string());
        let current = blocking(move || a.store.approval(&id2)).await?;
        if current.state != "pending" || tokio::time::Instant::now() >= deadline {
            return Ok(current);
        }
        // Wake-ups are hints; poll every second as well.
        let tick = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(1)));
        match wake.as_mut() {
            Some(w) => {
                tokio::select! {
                    _ = w.changed() => {}
                    _ = tick => {}
                }
            }
            None => tick.await,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn post_process(
    ctx: &Ctx,
    row: &Row,
    action: Action,
    scope: &Arc<Scope>,
    resp: Response,
    filter_list: bool,
    redact_fields: bool,
    place: Option<(&'static str, &'static str, String)>,
) -> Response {
    let (mut parts, body) = resp.into_parts();
    let is_json = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
    let bytes = match axum::body::to_bytes(body, MAX_ANSWER).await {
        Ok(b) => b,
        Err(_) => return deny(StatusCode::BAD_GATEWAY, "too_large", "the answer was too large to check".into(), json!({})),
    };
    let Some(mut v) = is_json.then(|| serde_json::from_slice::<Value>(&bytes).ok()).flatten() else {
        // Not JSON: nothing to filter. A list or a credential we can't see into isn't passed on.
        if filter_list || redact_fields {
            return deny(StatusCode::FORBIDDEN, "unfilterable", "this answer can't be checked for this token".into(), json!({}));
        }
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Some((kind, id_field, project)) = place
        && let Some(id) = v.get(id_field).and_then(Value::as_str).map(str::to_string)
    {
        let a = ctx.access.clone();
        let r = blocking(move || a.resolvers.workspace.as_ref().ok_or_else(|| Error::Internal("no workspace".into()))?.place(&project, kind, &id)).await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "putting a new resource in its project");
            // Made, but out of the token's reach: say so rather than leave it a mystery.
            if let Some(o) = v.as_object_mut() {
                o.insert("workspace_error".into(), Value::String(format!("created, but not put in the workspace project: {e}")));
            }
        }
    }
    if filter_list && let Tgt::List { items, id_field, kind } = row.target {
        let (a, s, product, prefix) = (ctx.access.clone(), scope.clone(), row.product, ctx.list_prefix.clone());
        let kind = kind.unwrap_or(product);
        v = tokio::task::spawn_blocking(move || {
            let keep = |item: &Value| {
                let Some(id) = item.get(id_field).and_then(Value::as_str) else { return false };
                let named = prefix.as_ref().map(|p| format!("{p}/{id}"));
                let id = named.as_deref().unwrap_or(id);
                let r = if product == "workspace" { Resource { project: Some(id.into()), ..Resource::new("workspace", id) } } else { a.resource(product, kind, id) };
                s.outcome(&Target::One(r), action) == Outcome::Allow
            };
            let filter = |list: &mut Vec<Value>| list.retain(&keep);
            match (items, &mut v) {
                (Some(k), _) => {
                    if let Some(Value::Array(l)) = v.get_mut(k) {
                        filter(l);
                    }
                }
                (None, Value::Array(l)) => filter(l),
                // `{"databases": [...], ...}`: every list of items with ids.
                (None, Value::Object(o)) => {
                    for x in o.values_mut() {
                        if let Value::Array(l) = x
                            && l.iter().any(|i| i.get(id_field).is_some())
                        {
                            filter(l);
                        }
                    }
                }
                _ => {}
            }
            v
        })
        .await
        .unwrap_or(Value::Null);
    }
    if redact_fields {
        redact::fields(&mut v, row.reveals);
    }
    let out = serde_json::to_vec(&v).unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(out))
}
