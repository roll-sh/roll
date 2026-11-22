//! MCP Events (`io.modelcontextprotocol/events`, SEP-3415; docs/mcp.md, "Events"), poll mode, for
//! 2026-07-28 clients: an agent asks what happened after a cursor (a deploy failing, a crash loop)
//! instead of being asked. An event type is a feed route read through the gated router with the
//! caller's token, as tools are, and named after the tool for that route: it's listed only where
//! the tool is, the gate decides and audits each poll, and lockdown withholds what the tool's
//! answer would. roll keeps nothing per client: the cursor is the feed's own position.

use crate::{Ctx, tools};
use roll_auth::lockdown;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const EXT: &str = "io.modelcontextprotocol/events";
/// How long a client waits before its next poll when nothing more is waiting.
const POLL_MS: u64 = 15_000;
/// What one poll reads from the feed, at most.
const PAGE: usize = 100;
/// What one poll answers, at most (a client may ask for fewer).
const MAX_EVENTS: usize = 100;
/// Cursors carry their feed's version, so a change of feed can tell an old one.
const CURSOR: &str = "a1:";

/// An event type and the feed it reads.
struct Feed {
    name: &'static str,
    description: &'static str,
    /// The tool for the feed's route: its rules, listing and lockdown are the type's.
    tool: &'static str,
    /// The feed's route, after a cursor (`{after}`).
    after: &'static str,
    /// The feed's route answering only its newest cursor (a client starting from now).
    latest: &'static str,
}

const FEEDS: &[Feed] = &[Feed {
    name: "apps.event",
    description: "Something happened to a service: a deploy going live or failing, a crash loop, a failed cron run or job, a memory, error, latency or capacity alert, a domain or a check, or an audit.* change to its settings. Filter by kinds (deploy.failed, or alert.* for a family), service id and environment.",
    tool: "apps_events",
    after: "/v1/apps/events?limit=100&after=",
    latest: "/v1/apps/events?latest=1",
}];

fn feed(name: &str) -> Option<&'static Feed> {
    FEEDS.iter().find(|f| f.name == name)
}

fn tool(f: &Feed) -> Option<&'static tools::Tool> {
    tools::all().iter().find(|t| t.name == f.tool)
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kinds": { "type": "array", "items": { "type": "string" }, "maxItems": 30, "description": "Only these kinds (deploy.failed, crash_loop), or a family ending in .* (alert.*); all when left out" },
            "service": { "type": "string", "description": "Only this service's (its id, svc_…)" },
            "environment": { "type": "string", "description": "Only this environment's (production)" },
        },
        "additionalProperties": false,
    })
}

fn payload_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "seq": { "type": "integer" },
            "at": { "type": "integer", "description": "Unix milliseconds" },
            "kind": { "type": "string" },
            "service": { "type": "string" },
            "environment": { "type": "string" },
            "deployment": { "type": ["string", "null"] },
            "data": { "description": "The kind's details" },
        },
    })
}

/// How this connection can have events: polled, and POSTed to a webhook where roll delivers them
/// (a server with a sealing key) and the connection can be looked up later (a token or OAuth).
fn delivery(ctx: &Ctx) -> Value {
    let hooks = subscriber(ctx).is_ok();
    if hooks { json!(["poll", "webhook"]) } else { json!(["poll"]) }
}

/// `events/list`: the types this connection may poll (its tool is listed for it).
pub(crate) fn list(ctx: &Ctx) -> Value {
    let events: Vec<Value> = FEEDS
        .iter()
        .filter(|f| tool(f).is_some_and(|t| ctx.usable(t)))
        .map(|f| json!({ "name": f.name, "description": f.description, "delivery": delivery(ctx), "inputSchema": input_schema(), "payloadSchema": payload_schema() }))
        .collect();
    json!({ "events": events })
}

/// A JSON-RPC error: its code, message and data.
pub type Refused = (i64, String, Value);

fn invalid(message: impl Into<String>) -> Refused {
    (-32602, message.into(), json!({}))
}

/// The filter a poll's `arguments` ask for.
struct Filter {
    kinds: Vec<String>,
    service: Option<String>,
    environment: Option<String>,
}

impl Filter {
    fn parse(args: &Value) -> Result<Filter, Refused> {
        let empty = Map::new();
        let o = match args {
            Value::Null => &empty,
            Value::Object(o) => o,
            _ => return Err(invalid("arguments is an object")),
        };
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "kinds" | "service" | "environment")) {
            return Err(invalid(format!("no argument {k:?}: kinds, service and environment")));
        }
        let text = |k: &str| match o.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.is_empty() && s.len() <= 200 => Ok(Some(s.clone())),
            _ => Err(invalid(format!("{k} is a string"))),
        };
        let kinds = match o.get("kinds") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) if a.len() <= 30 => a
                .iter()
                .map(|k| k.as_str().filter(|s| !s.is_empty() && s.len() <= 64).map(str::to_string).ok_or_else(|| invalid("kinds are strings")))
                .collect::<Result<_, _>>()?,
            _ => return Err(invalid("kinds is a list of at most 30")),
        };
        Ok(Filter { kinds, service: text("service")?, environment: text("environment")? })
    }

    fn keeps(&self, e: &Value) -> bool {
        let kind = e["kind"].as_str().unwrap_or("");
        (self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind || k.strip_suffix('*').is_some_and(|p| p.ends_with('.') && kind.starts_with(p))))
            && self.service.as_deref().is_none_or(|s| e["service"] == s)
            && self.environment.as_deref().is_none_or(|s| e["environment"] == s)
    }
}

/// The feed's answer to an error the client can act on: the gate's refusal is Forbidden, a product
/// that isn't running is NotFound, a rate limit is ResourceExhausted.
fn refused(f: &Feed, (text, data): (String, Value)) -> Refused {
    match data["http_status"].as_u64() {
        Some(401 | 403) => (-32024, "Forbidden".into(), json!({ "reason": text })),
        Some(404) => (-32023, format!("{} isn't available here", f.name), json!({ "kind": "event" })),
        Some(429) => (-32025, "ResourceExhausted".into(), json!({ "limit": "rate", "retry_after": data["retry_after"] })),
        _ => (-32603, text, json!({})),
    }
}

/// `events/poll`: one subscription per request (its name, arguments and cursor); nothing kept.
pub(crate) async fn poll(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, Refused> {
    let name = params["name"].as_str().unwrap_or("");
    let Some(f) = feed(name) else {
        return Err((-32023, format!("no event {name:?}: events/list says which there are"), json!({ "kind": "event" })));
    };
    // Its tool isn't this connection's (the token's rules, the URL's toolsets, read-only).
    if !tool(f).is_some_and(|t| ctx.usable(t)) {
        return Err((-32024, "Forbidden".into(), json!({ "reason": format!("this connection can't use {}", f.tool) })));
    }
    let filter = Filter::parse(&params["arguments"])?;
    let max = match &params["maxEvents"] {
        Value::Null => 50,
        v => v.as_u64().filter(|n| (1..=MAX_EVENTS as u64).contains(n)).ok_or_else(|| invalid(format!("maxEvents is 1-{MAX_EVENTS}")))? as usize,
    };
    let floor = match &params["maxAgeMs"] {
        Value::Null => None,
        v => Some(crate::code_sql_now() - v.as_i64().filter(|n| *n >= 0).ok_or_else(|| invalid("maxAgeMs is a number of milliseconds"))?),
    };
    let audit = json!({ "name": name, "arguments": params["arguments"] });
    let what = format!("events/poll:{name}");
    let after = match &params["cursor"] {
        Value::Null => None,
        Value::String(c) => c.strip_prefix(CURSOR).and_then(|n| n.parse::<i64>().ok()).filter(|n| *n >= 0).map(Some).unwrap_or(Some(-1)),
        _ => return Err(invalid("cursor is a string from the last poll, or null")),
    };
    // No cursor: start from now (nothing yet, the newest cursor). One this feed can't read (another
    // version's, made up): a fresh one, and a possible gap.
    let truncated = after == Some(-1);
    let Some(after) = after.filter(|n| *n >= 0) else {
        let page = crate::read_route_as(ctx, f.latest, &what, audit, false).await.map_err(|e| refused(f, e))?;
        let next = page["next"].as_i64().unwrap_or(0);
        return Ok(json!({ "events": [], "cursor": format!("{CURSOR}{next}"), "truncated": truncated, "hasMore": false, "nextPollMs": POLL_MS }));
    };
    let page = crate::read_route_as(ctx, &format!("{}{after}", f.after), &what, audit, false).await.map_err(|e| refused(f, e))?;
    let read = page["events"].as_array().cloned().unwrap_or_default();
    let outside = tool(f).map_or(&[][..], |t| t.outside);
    let (mut events, mut cursor, mut aged) = (Vec::new(), after, false);
    let mut more = read.len() >= PAGE;
    for mut e in read {
        let seq = e["seq"].as_i64().unwrap_or(cursor);
        if events.len() == max {
            // Full: the next poll starts after the last one answered, at once.
            more = true;
            break;
        }
        cursor = seq;
        if floor.is_some_and(|fl| e["at"].as_i64().unwrap_or(0) < fl) {
            aged = true;
            continue;
        }
        if !filter.keeps(&e) {
            continue;
        }
        // Event payloads are untrusted, as tool results are: lockdown withholds what the tool's would.
        if ctx.lockdown {
            lockdown::withhold(&mut e);
            lockdown::withhold_fields(&mut e, outside);
        }
        let at = e["at"].as_i64().unwrap_or(0);
        events.push(json!({ "eventId": format!("{}:{seq}", f.name), "name": f.name, "timestamp": iso8601(at), "data": e }));
    }
    if !more {
        cursor = page["next"].as_i64().unwrap_or(cursor).max(cursor);
    }
    Ok(json!({ "events": events, "cursor": format!("{CURSOR}{cursor}"), "truncated": aged, "hasMore": more, "nextPollMs": POLL_MS }))
}

/// The connection a webhook subscription is for: a token's or an OAuth connection's grant (the
/// SEP needs an authenticated principal; roll needs one it can look up as it is now, so never the
/// operator's own or a person's session, which is made per request).
fn subscriber(ctx: &Ctx) -> Result<(Arc<roll_access::Access>, String), Refused> {
    let forbidden = |why: &str| (-32024, "Forbidden".to_string(), json!({ "reason": why }));
    let access = ctx.m.config.access.clone().filter(|a| a.event_hooks.get().is_some()).ok_or_else(|| (-32026, "Unsupported".to_string(), json!({ "feature": "deliveryMode", "value": "webhook" })))?;
    match &ctx.principal {
        crate::Principal::Grant(g) if matches!(g.kind, roll_auth::GrantKind::Token | roll_auth::GrantKind::Oauth) => Ok((access, g.id.to_string())),
        _ => Err(forbidden("webhook subscriptions are a token's or an OAuth connection's: connect with one")),
    }
}

/// `f` with `access` off the async threads (the store is SQLite or Postgres, both blocking).
async fn blocking<T: Send + 'static>(access: &Arc<roll_access::Access>, f: impl FnOnce(&roll_access::Access) -> roll_access::Result<T> + Send + 'static) -> Result<T, Refused> {
    let a = access.clone();
    match tokio::task::spawn_blocking(move || f(&a)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(roll_access::Error::Forbidden(why))) => Err((-32024, "Forbidden".into(), json!({ "reason": why }))),
        Ok(Err(roll_access::Error::BadRequest(why))) => Err(invalid(why)),
        Ok(Err(roll_access::Error::TooMany(why))) => Err((-32025, why, json!({ "limit": "subscriptions", "max": roll_access::event_subs::PER_GRANT }))),
        Ok(Err(e)) => Err(internal(e)),
        Err(e) => Err(internal(e)),
    }
}

/// The parts of a subscribe or unsubscribe every check needs: its feed, arguments and callback URL.
fn keyed<'a>(ctx: &Ctx, params: &'a Value) -> Result<(&'static Feed, Value, &'a str), Refused> {
    let name = params["name"].as_str().unwrap_or("");
    let Some(f) = feed(name) else {
        return Err((-32023, format!("no event {name:?}: events/list says which there are"), json!({ "kind": "event" })));
    };
    if !tool(f).is_some_and(|t| ctx.usable(t)) {
        return Err((-32024, "Forbidden".into(), json!({ "reason": format!("this connection can't use {}", f.tool) })));
    }
    Filter::parse(&params["arguments"])?;
    let args = if params["arguments"].is_null() { json!({}) } else { params["arguments"].clone() };
    let url = params["delivery"]["url"].as_str().filter(|u| !u.is_empty() && u.len() <= 2048).ok_or_else(|| invalid("delivery.url is the callback's https URL"))?;
    Ok((f, args, url))
}

/// `events/subscribe` (webhook mode): roll POSTs what happens to the callback, signed with the
/// client's secret, until the subscription isn't refreshed. The feed is read once now as the caller
/// (what would refuse or ask a person refuses here, before anything is kept), and a callback proves
/// once a day per connection that it wants deliveries (it echoes a signed challenge).
pub(crate) async fn subscribe(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, Refused> {
    let (access, grant) = subscriber(ctx)?;
    let (f, args, url) = keyed(ctx, params)?;
    match params["delivery"]["mode"].as_str() {
        Some("webhook") => {}
        Some(m) => return Err((-32026, "Unsupported".into(), json!({ "feature": "deliveryMode", "value": m }))),
        None => return Err(invalid("delivery.mode is webhook")),
    }
    let u = url.to_string();
    blocking(&access, move |a| a.event_url_ok(&u)).await?;
    let secret = params["delivery"]["secret"].as_str().unwrap_or("").to_string();
    if roll_access::event_subs::secret_key(&secret).is_none() {
        return Err(invalid("delivery.secret is whsec_ and the base64 of 24 to 64 random bytes"));
    }
    let ttl = match &params["ttlMs"] {
        Value::Null => roll_access::event_subs::MAX_TTL_MS,
        v => v.as_i64().filter(|n| *n > 0).ok_or_else(|| invalid("ttlMs is a number of milliseconds"))?,
    };
    // Its grant may have them (not one held to some addresses), and the feed answers it now.
    let g = grant.clone();
    blocking(&access, move |a| a.event_subscriber_ok(&g)).await?;
    let probe = crate::read_route_as(ctx, f.latest, &format!("events/subscribe:{}", f.name), json!({ "name": f.name, "arguments": args }), true).await.map_err(|e| refused(f, e))?;
    let (g, u, n, a2) = (grant.clone(), url.to_string(), f.name, args.clone());
    let existing = blocking(&access, move |a| a.event_sub(&g, &u, n, &a2)).await?;
    // A new one starts where the client says, or now; a refresh keeps the cursor roll delivered to.
    let (cursor, truncated) = match (&existing, &params["cursor"]) {
        (Some(e), _) => (e.cursor.clone(), false),
        (None, Value::String(c)) if c.strip_prefix(CURSOR).is_some_and(|n| n.parse::<i64>().is_ok_and(|n| n >= 0)) => (c.clone(), false),
        (None, c) => (format!("{CURSOR}{}", probe["next"].as_i64().unwrap_or(0)), c.is_string()),
    };
    if existing.is_none() {
        let g = grant.clone();
        if blocking(&access, move |a| a.event_subs_held(&g)).await? >= roll_access::event_subs::PER_GRANT {
            return Err((-32025, "ResourceExhausted".into(), json!({ "limit": "subscriptions", "max": roll_access::event_subs::PER_GRANT })));
        }
    }
    let id = roll_access::event_subs::sub_id(&grant, url, f.name, &args);
    let (g, u) = (grant.clone(), url.to_string());
    if !blocking(&access, move |a| a.event_receiver_verified(&g, &u)).await?
        && let Err(reason) = access.verify_event_receiver(&grant, url, &secret, &id).await
    {
        let (a, g, u) = (access.clone(), grant.clone(), url.to_string());
        let _ = tokio::task::spawn_blocking(move || a.audit_event_sub(&g, "verify", n, &u, "deny")).await;
        if reason == "rate" {
            return Err((-32025, "ResourceExhausted".into(), json!({ "limit": "verifications" })));
        }
        return Err((-32027, "CallbackEndpointError".into(), json!({ "reason": reason })));
    }
    let lockdown = ctx.lockdown;
    let (g, u, a2, sec, cur) = (grant.clone(), url.to_string(), args.clone(), secret.clone(), cursor.clone());
    let refreshed = existing.is_some();
    let sub = blocking(&access, move |a| {
        let new = roll_access::event_subs::NewEventSub { grant_id: &g, name: n, arguments: &a2, url: &u, secret: &sec, cursor: &cur, lockdown, ttl_ms: ttl };
        let sub = a.put_event_sub(&new)?;
        a.audit_event_sub(&g, "subscribe", n, &u, if refreshed { "refresh" } else { "allow" });
        Ok(sub)
    })
    .await?;
    let mut out = json!({ "id": sub.id, "refreshBefore": iso8601(sub.expires_at), "cursor": sub.cursor, "truncated": truncated });
    if refreshed {
        out["deliveryStatus"] = json!({
            "active": sub.last_error.is_none(),
            "lastDeliveryAt": sub.last_delivery_at.map(iso8601),
            "lastError": sub.last_error,
            "failedSince": sub.failed_since.map(iso8601),
        });
    }
    Ok(out)
}

/// `events/unsubscribe`: this connection's subscription with that key ends now.
pub(crate) async fn unsubscribe(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, Refused> {
    let (access, grant) = subscriber(ctx)?;
    let name = params["name"].as_str().unwrap_or("");
    let f = feed(name).ok_or_else(|| (-32023, format!("no event {name:?}"), json!({ "kind": "event" })))?;
    let args = if params["arguments"].is_null() { json!({}) } else { params["arguments"].clone() };
    let url = params["delivery"]["url"].as_str().ok_or_else(|| invalid("delivery.url names the subscription"))?.to_string();
    let n = f.name;
    let ended = blocking(&access, move |a| {
        let ended = a.delete_event_sub(&grant, &url, n, &args)?;
        if ended {
            a.audit_event_sub(&grant, "unsubscribe", n, &url, "allow");
        }
        Ok(ended)
    })
    .await?;
    if ended { Ok(json!({})) } else { Err((-32023, "no such subscription of this connection's".into(), json!({ "kind": "subscription" }))) }
}

/// Events one round delivers to a subscription, at most (the next round goes on from there).
const DELIVER_MAX: usize = 20;
/// Subscriptions one node claims a round, and delivers to at once: with each one's time budget,
/// a round ends well inside its claims' hold.
const CLAIM_MAX: i64 = 16;
const AT_ONCE: usize = 8;
/// One subscription's time in a round: past it, what's delivered so far is kept and the rest waits.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(40);

/// What a delivery round did: subscriptions claimed, events delivered, deliveries that failed,
/// subscriptions ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Delivered {
    pub claimed: usize,
    pub delivered: usize,
    pub failed: usize,
    pub ended: usize,
}

/// One delivery round (rolld runs one every 15 seconds on each node; a claim keeps two nodes from
/// delivering one subscription, and only the claim's holder writes back). Each due subscription to
/// an event whose product runs here (`products`) has its feed read through the gated router as its
/// grant (`roll_auth::ActAs`: as it is now, narrowed to reading, decided and audited as
/// `events/deliver:<name>`, quiet, so what would ask a person is refused); a refusal ends it with
/// a signed `terminated` envelope, and otherwise what its filter keeps is POSTed in order, the
/// cursor moving past what the callback took.
pub async fn deliver(api: axum::Router, access: Arc<roll_access::Access>, products: &[String]) -> Delivered {
    use futures::StreamExt;
    let names: Vec<&'static str> = FEEDS.iter().filter(|f| tool(f).is_some_and(|t| products.iter().any(|p| p == t.product))).map(|f| f.name).collect();
    let a = access.clone();
    let Ok(Ok(claimed)) = tokio::task::spawn_blocking(move || a.claim_event_subs(CLAIM_MAX, &names)).await else { return Delivered::default() };
    let n = claimed.len();
    // Each in a task of its own: a handler that panics ends that subscription's round, not the loop.
    let rounds = futures::stream::iter(claimed.into_iter().map(|c| tokio::spawn(one(api.clone(), access.clone(), c)))).buffer_unordered(AT_ONCE).collect::<Vec<_>>().await;
    rounds.into_iter().flatten().fold(Delivered { claimed: n, ..Default::default() }, |t, d| Delivered { claimed: t.claimed, delivered: t.delivered + d.delivered, failed: t.failed + d.failed, ended: t.ended + d.ended })
}

/// A round's write about its subscription, off the async threads.
type WriteBack = Box<dyn FnOnce(&roll_access::Access) -> roll_access::Result<()> + Send>;

/// One subscription's part of a round.
async fn one(api: axum::Router, access: Arc<roll_access::Access>, c: roll_access::event_subs::Claimed) -> Delivered {
    let started = std::time::Instant::now();
    let mut out = Delivered::default();
    let sub = &c.sub;
    let write = |f: WriteBack| {
        let a = access.clone();
        async move {
            let _ = tokio::task::spawn_blocking(move || f(&a)).await;
        }
    };
    if c.keys.is_empty() {
        write(Box::new(move |a| a.drop_event_sub(&c))).await;
        out.ended = 1;
        return out;
    }
    let Some(f) = feed(&sub.name) else {
        terminate(&access, c, -32023, "NotFound", json!({ "kind": "event" })).await;
        out.ended = 1;
        return out;
    };
    let after = sub.cursor.strip_prefix(CURSOR).and_then(|n| n.parse::<i64>().ok()).unwrap_or(0);
    let mut req = axum::http::Request::builder().method("GET").uri(format!("{}{after}", f.after)).body(axum::body::Body::empty()).expect("a feed route is a URI");
    req.extensions_mut().insert(roll_auth::ActAs(sub.grant_id.as_str().into()));
    let label: Arc<str> = format!("events/deliver:{}", f.name).into();
    req.extensions_mut().insert(roll_access::evaluate::McpCall { tool: label, args: Arc::new(json!({ "name": f.name, "arguments": sub.arguments })), ip: None, elicits: false, client: None, quiet: true, unattended: true });
    let resp = match tower::ServiceExt::oneshot(api, req).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    let status = resp.status();
    let page: Value = axum::body::to_bytes(resp.into_body(), crate::MAX_ANSWER).await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    match status.as_u16() {
        200..=299 => {}
        // Its grant can't read the feed any more: revoked, expired, narrowed, a policy's.
        401 | 403 => {
            terminate(&access, c, -32024, "Forbidden", json!({ "reason": "access ended" })).await;
            out.ended = 1;
            return out;
        }
        // The product is busy (or off here a moment): the next round tries again, its state kept.
        _ => {
            write(Box::new(move |a| a.event_sub_retry(&c))).await;
            return out;
        }
    }
    let filter = Filter::parse(&sub.arguments).unwrap_or(Filter { kinds: Vec::new(), service: None, environment: None });
    let outside = tool(f).map_or(&[][..], |t| t.outside);
    // Lockdown as the subscription asked, or as the policy holds every connection now.
    let a = access.clone();
    let lockdown = sub.lockdown || tokio::task::spawn_blocking(move || a.policy_now().mcp_lockdown).await.unwrap_or(true);
    let read = page["events"].as_array().cloned().unwrap_or_default();
    let (mut cursor, mut sent, mut error, mut gone, mut cut) = (after, 0, None, false, false);
    let full = read.len() >= PAGE;
    for mut e in read {
        let seq = e["seq"].as_i64().unwrap_or(cursor);
        if sent == DELIVER_MAX || started.elapsed() > BUDGET {
            cut = true;
            break;
        }
        if !filter.keeps(&e) {
            cursor = seq;
            continue;
        }
        if lockdown {
            lockdown::withhold(&mut e);
            lockdown::withhold_fields(&mut e, outside);
        }
        let at = e["at"].as_i64().unwrap_or(0);
        let body = serde_json::to_vec(&json!({ "eventId": format!("{}:{seq}", f.name), "name": f.name, "timestamp": iso8601(at), "data": e, "cursor": format!("{CURSOR}{seq}") })).unwrap_or_default();
        let id = roll_access::event_subs::event_msg_id(&sub.id, seq);
        match access.post_event(&c, &id, body).await {
            Ok(200..=299) => {
                (cursor, sent) = (seq, sent + 1);
                out.delivered += 1;
            }
            // Gone: the receiver wants no more (non-retryable). Too large: this one is skipped.
            Ok(410) => {
                gone = true;
                break;
            }
            Ok(413) => cursor = seq,
            Ok(s) => {
                error = Some(roll_access::event_subs::endpoint_error(&format!("http_{s}")).to_string());
                break;
            }
            Err(category) => {
                error = Some(category);
                break;
            }
        }
    }
    if gone {
        write(Box::new(move |a| a.drop_event_sub(&c))).await;
        out.ended = 1;
        return out;
    }
    // Read to the end with nothing left over: the cursor moves to the feed's own next.
    if error.is_none() && !full && !cut {
        cursor = page["next"].as_i64().unwrap_or(cursor).max(cursor);
    }
    out.failed = usize::from(error.is_some());
    let at = format!("{CURSOR}{cursor}");
    let more = error.is_none() && (full || cut);
    write(Box::new(move |a| a.event_sub_done(&c, &at, sent > 0, error.as_deref(), more))).await;
    out
}

/// End a subscription with its signed `terminated` envelope (best effort: it ends either way).
async fn terminate(access: &Arc<roll_access::Access>, c: roll_access::event_subs::Claimed, code: i64, message: &str, data: Value) {
    let body = serde_json::to_vec(&json!({ "type": "terminated", "error": { "code": code, "message": message, "data": data } })).unwrap_or_default();
    let _ = access.post_event(&c, &roll_access::event_subs::msg_id("terminated"), body).await;
    let a = access.clone();
    let _ = tokio::task::spawn_blocking(move || a.drop_event_sub(&c)).await;
}

/// rolld's delivery loop: a round every 15 seconds where roll can deliver (a sealing key), for the
/// events whose products run on this node.
pub fn spawn_deliveries(api: axum::Router, access: Arc<roll_access::Access>, products: Vec<String>) {
    if access.event_hooks.get().is_none() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // Claims again while every claim found more due, for up to a minute: a busy node keeps
            // up instead of waiting a tick per 16.
            let started = std::time::Instant::now();
            loop {
                let d = deliver(api.clone(), access.clone(), &products).await;
                if d.failed > 0 {
                    tracing::info!(delivered = d.delivered, failed = d.failed, ended = d.ended, "MCP event webhooks");
                }
                if d.claimed < CLAIM_MAX as usize || started.elapsed() > std::time::Duration::from_secs(60) {
                    break;
                }
            }
        }
    });
}

/// An error of roll's own: the client hears that much, the log the rest.
fn internal(e: impl std::fmt::Display) -> Refused {
    tracing::warn!(error = %e, "MCP events");
    (-32603, "internal error".into(), json!({}))
}

/// Unix milliseconds as ISO 8601 in UTC (`2026-11-22T09:30:00.123Z`).
fn iso8601(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn times_are_iso8601() {
        assert_eq!(super::iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::iso8601(1_795_334_400_123), "2026-11-22T08:00:00.123Z");
    }
}
