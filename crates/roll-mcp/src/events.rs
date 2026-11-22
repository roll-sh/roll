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
    /// The tool for the feed's route: its rules, listing and lockdown are the type's (roll's own,
    /// or a fronted set's: roll code's).
    tool: &'static str,
    /// The product whose route it reads (deliveries run where it does).
    product: &'static str,
    /// Arguments that fill the routes' path (`{owner}`, `{repo}`): required, never filters.
    path: &'static [(&'static str, &'static str)],
    /// The feed's route, after a cursor (`{after}`).
    after: &'static str,
    /// The feed's route answering only its newest cursor (a client starting from now).
    latest: &'static str,
    /// The arguments that filter it: `kinds` a list matched against its field (exact, or a family
    /// as `x.*`), any other one value its field must equal; with what each means.
    filters: &'static [(&'static str, &'static str, &'static str)],
    /// What an event's `data` holds.
    payload: fn() -> Value,
}

const FEEDS: &[Feed] = &[
    Feed {
        name: "apps.event",
        description: "Something happened to a service: a deploy going live or failing, a crash loop, a failed cron run or job, a memory, error, latency or capacity alert, a domain or a check, or an audit.* change to its settings. Filter by kinds (deploy.failed, or alert.* for a family), service id and environment.",
        tool: "apps_events",
        product: "apps",
        path: &[],
        after: "/v1/apps/events?limit=100&after=",
        latest: "/v1/apps/events?latest=1",
        filters: &[
            ("kinds", "kind", "Only these kinds (deploy.failed, crash_loop), or a family ending in .* (alert.*); all when left out"),
            ("service", "service", "Only this service's (its id, svc_…)"),
            ("environment", "environment", "Only this environment's (production)"),
        ],
        payload: || {
            json!({ "type": "object", "properties": {
                "seq": { "type": "integer" },
                "at": { "type": "integer", "description": "Unix milliseconds" },
                "kind": { "type": "string" },
                "service": { "type": "string" },
                "environment": { "type": "string" },
                "deployment": { "type": ["string", "null"] },
                "data": { "description": "The kind's details" },
            } })
        },
    },
    Feed {
        name: "previews.event",
        description: "Something happened to a preview: created, ready (up after a push, never a sleeping one waking), failed, a visual change waiting for review, or deleted. Filter by kinds and project; only your projects' previews.",
        tool: "previews_list",
        product: "previews",
        path: &[],
        after: "/v1/previews/events?limit=100&after=",
        latest: "/v1/previews/events?latest=1",
        filters: &[("kinds", "kind", "Only these kinds (ready, failed, visual_change, created, deleted); all when left out"), ("project", "project", "Only this workspace project's")],
        payload: || {
            json!({ "type": "object", "properties": {
                "seq": { "type": "integer" },
                "at": { "type": "integer", "description": "Unix milliseconds" },
                "kind": { "type": "string" },
                "project": { "type": "string" },
                "preview": { "type": "string", "description": "The preview's id: previews_get has its issues and URLs" },
                "change": { "type": ["string", "null"] },
                "data": { "type": "object", "description": "head: the commit it was built from" },
            } })
        },
    },
    Feed {
        name: "access.approval",
        description: "A person decided an approval this connection asked for (approved or denied): call the original tool again with approval once it's approved. Filter by kinds (approved, denied) and tool.",
        tool: "access_wait_approval",
        product: "access",
        path: &[],
        after: "/v1/access/approvals/decided?limit=100&after=",
        latest: "/v1/access/approvals/decided?latest=1",
        filters: &[("kinds", "state", "Only these outcomes (approved, denied); all when left out"), ("tool", "tool", "Only approvals of calls to this tool")],
        payload: || {
            json!({ "type": "object", "properties": {
                "seq": { "type": "integer" },
                "at": { "type": ["integer", "null"], "description": "When it was decided, Unix milliseconds" },
                "id": { "type": "string", "description": "The approval (apr_…): pass it as the tool call's approval" },
                "state": { "type": "string", "description": "approved or denied" },
                "product": { "type": "string" },
                "action": { "type": "string" },
                "resource": { "type": ["string", "null"] },
                "tool": { "type": ["string", "null"] },
                "summary": { "type": "string" },
                "decided_by": { "type": ["string", "null"] },
                "reason": { "type": ["string", "null"] },
            } })
        },
    },
    Feed {
        name: "code.landed",
        description: "A change landed on a roll code repo (its default branch or another ref): the change, the commit and what it brought. Give the repo by owner and repo; filter by ref.",
        tool: "code_list_refs",
        product: "code",
        path: &[("owner", "The repo's owner"), ("repo", "The repo's name")],
        after: "/v1/code/repos/{owner}/{repo}/landings?limit=100&after=",
        latest: "/v1/code/repos/{owner}/{repo}/landings?latest=1",
        filters: &[("ref", "ref", "Only landings on this ref (refs/heads/main)")],
        payload: || {
            json!({ "type": "object", "properties": {
                "seq": { "type": "integer" },
                "at": { "type": "integer", "description": "Unix milliseconds" },
                "kind": { "type": "string" },
                "change": { "type": ["string", "null"], "description": "The change that landed (get_change)" },
                "ref": { "type": "string" },
            } })
        },
    },
];

fn feed(name: &str) -> Option<&'static Feed> {
    FEEDS.iter().find(|f| f.name == name)
}

fn tool(f: &Feed) -> Option<&'static tools::Tool> {
    tools::all().iter().find(|t| t.name == f.tool)
}

/// Whether this connection has the feed's tool (roll's own, or a fronted set's it's offered).
fn usable(ctx: &Ctx, f: &Feed) -> bool {
    match tool(f) {
        Some(t) => ctx.usable(t),
        None => ctx.usable_tools().iter().any(|t| t["name"] == f.tool),
    }
}

/// A subscription's feed as its arguments give it: the filter, and the routes with the path filled.
struct Opened {
    filter: Filter,
    /// The route after a cursor (the cursor's number goes at its end).
    after: String,
    latest: String,
}

/// Check a subscription's arguments against its feed, and fill the routes' path from them.
fn open(f: &Feed, args: &Value) -> Result<Opened, Refused> {
    let filter = Filter::parse(f, args)?;
    let (mut after, mut latest) = (f.after.to_string(), f.latest.to_string());
    for (arg, _) in f.path {
        let v = args[*arg].as_str().filter(|v| !v.is_empty() && v.len() <= 200).ok_or_else(|| invalid(format!("{arg} is required")))?;
        let seg = percent_encoding::utf8_percent_encode(v, tools::SEGMENT).to_string();
        after = after.replace(&format!("{{{arg}}}"), &seg);
        latest = latest.replace(&format!("{{{arg}}}"), &seg);
    }
    Ok(Opened { filter, after, latest })
}

fn input_schema(f: &Feed) -> Value {
    let mut properties: Map<String, Value> = f.path.iter().map(|(arg, about)| (arg.to_string(), json!({ "type": "string", "description": about }))).collect();
    properties.extend(f.filters.iter().map(|(arg, _, about)| {
        let schema = if *arg == "kinds" { json!({ "type": "array", "items": { "type": "string" }, "maxItems": 30, "description": about }) } else { json!({ "type": "string", "description": about }) };
        (arg.to_string(), schema)
    }));
    let required: Vec<&str> = f.path.iter().map(|(arg, _)| *arg).collect();
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// How this connection can have events: polled, and POSTed to a webhook where roll delivers them
/// (a server with a sealing key) and the connection can be looked up later (a token or OAuth).
fn delivery(ctx: &Ctx) -> Value {
    let hooks = subscriber(ctx).is_ok();
    if hooks { json!(["poll", "push", "webhook"]) } else { json!(["poll", "push"]) }
}

/// `events/list`: the types this connection may poll (its tool is listed for it).
pub(crate) fn list(ctx: &Ctx) -> Value {
    let events: Vec<Value> = FEEDS
        .iter()
        .filter(|f| usable(ctx, f))
        .map(|f| json!({ "name": f.name, "description": f.description, "delivery": delivery(ctx), "inputSchema": input_schema(f), "payloadSchema": (f.payload)() }))
        .collect();
    json!({ "events": events })
}

/// A JSON-RPC error: its code, message and data.
pub type Refused = (i64, String, Value);

fn invalid(message: impl Into<String>) -> Refused {
    (-32602, message.into(), json!({}))
}

/// The filter a poll's `arguments` ask for, over its feed's fields.
struct Filter {
    kinds: Vec<String>,
    kinds_field: &'static str,
    exact: Vec<(&'static str, String)>,
}

impl Filter {
    /// Nothing filtered out.
    fn none() -> Filter {
        Filter { kinds: Vec::new(), kinds_field: "", exact: Vec::new() }
    }

    fn parse(f: &Feed, args: &Value) -> Result<Filter, Refused> {
        let empty = Map::new();
        let o = match args {
            Value::Null => &empty,
            Value::Object(o) => o,
            _ => return Err(invalid("arguments is an object")),
        };
        if let Some(k) = o.keys().find(|k| !f.filters.iter().any(|(arg, ..)| arg == k) && !f.path.iter().any(|(arg, _)| arg == k)) {
            let known: Vec<&str> = f.path.iter().map(|(arg, _)| *arg).chain(f.filters.iter().map(|(arg, ..)| *arg)).collect();
            return Err(invalid(format!("no argument {k:?}: {}", known.join(", "))));
        }
        let mut out = Filter::none();
        for (arg, field, _) in f.filters {
            match (o.get(*arg), *arg) {
                (None | Some(Value::Null), _) => {}
                (Some(Value::Array(a)), "kinds") if a.len() <= 30 => {
                    out.kinds = a.iter().map(|k| k.as_str().filter(|s| !s.is_empty() && s.len() <= 64).map(str::to_string).ok_or_else(|| invalid("kinds are strings"))).collect::<Result<_, _>>()?;
                    out.kinds_field = field;
                }
                (_, "kinds") => return Err(invalid("kinds is a list of at most 30")),
                (Some(Value::String(v)), _) if !v.is_empty() && v.len() <= 200 => out.exact.push((field, v.clone())),
                _ => return Err(invalid(format!("{arg} is a string"))),
            }
        }
        Ok(out)
    }

    fn keeps(&self, e: &Value) -> bool {
        let kind = e[self.kinds_field].as_str().unwrap_or("");
        (self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind || k.strip_suffix('*').is_some_and(|p| p.ends_with('.') && kind.starts_with(p)))) && self.exact.iter().all(|(field, v)| e[*field] == v.as_str())
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
    if !usable(ctx, f) {
        return Err((-32024, "Forbidden".into(), json!({ "reason": format!("this connection can't use {}", f.tool) })));
    }
    let opened = open(f, &params["arguments"])?;
    let filter = &opened.filter;
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
        let page = crate::read_route_as(ctx, &opened.latest, &what, audit, false).await.map_err(|e| refused(f, e))?;
        let next = page["next"].as_i64().unwrap_or(0);
        return Ok(json!({ "events": [], "cursor": format!("{CURSOR}{next}"), "truncated": truncated, "hasMore": false, "nextPollMs": POLL_MS }));
    };
    let page = crate::read_route_as(ctx, &format!("{}{after}", opened.after), &what, audit, false).await.map_err(|e| refused(f, e))?;
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
    if !usable(ctx, f) {
        return Err((-32024, "Forbidden".into(), json!({ "reason": format!("this connection can't use {}", f.tool) })));
    }
    open(f, &params["arguments"])?;
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
    let latest = open(f, &args)?.latest;
    let probe = crate::read_route_as(ctx, &latest, &format!("events/subscribe:{}", f.name), json!({ "name": f.name, "arguments": args }), true).await.map_err(|e| refused(f, e))?;
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
    // Access and workspace run on every node (they're never in the list of products that are on).
    let names: Vec<&'static str> = FEEDS.iter().filter(|f| matches!(f.product, "access" | "workspace") || products.iter().any(|p| p == f.product)).map(|f| f.name).collect();
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
    let Ok(opened) = open(f, &sub.arguments) else {
        terminate(&access, c, -32026, "Unsupported", json!({ "feature": "inputSchema", "reason": "schema_changed" })).await;
        out.ended = 1;
        return out;
    };
    let after = sub.cursor.strip_prefix(CURSOR).and_then(|n| n.parse::<i64>().ok()).unwrap_or(0);
    let mut req = axum::http::Request::builder().method("GET").uri(format!("{}{after}", opened.after)).body(axum::body::Body::empty()).expect("a feed route is a URI");
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
    let filter = opened.filter;
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

/// Event streams one token may hold open on a node.
const STREAMS_PER_TOKEN: usize = 10;
/// How often an open stream reads its feed (each read the caller's request, in the audit log).
const STREAM_READ_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
/// The heartbeat a stream grants: what the client asks, within these (30 s if it doesn't say).
const HEARTBEAT_MIN_MS: u64 = 15_000;
const HEARTBEAT_MAX_MS: u64 = 120_000;
/// How long a stream stays open: then roll closes it with its result, and the client reconnects
/// with its cursor (a token's rules change, a node drains).
const STREAM_FOR: std::time::Duration = std::time::Duration::from_secs(3600);

/// What one read of a feed gave a subscription: its notifications, where it got to, whether more
/// is waiting, and whether `maxAgeMs` skipped any.
struct Read {
    out: Vec<Value>,
    cursor: i64,
    more: bool,
    aged: bool,
}

/// Read a feed after `after` as the caller (unattended: nothing asks a person) and turn what the
/// filter keeps into event notifications, each with its cursor.
async fn read_feed(ctx: &Arc<Ctx>, f: &'static Feed, route: &str, filter: &Filter, after: i64, floor: Option<i64>, meta: &Value) -> Result<Read, Refused> {
    let audit = json!({ "name": f.name });
    let page = crate::read_route_as(ctx, &format!("{route}{after}"), &format!("events/stream:{}", f.name), audit, true).await.map_err(|e| refused(f, e))?;
    let events = page["events"].as_array().cloned().unwrap_or_default();
    let more = events.len() >= PAGE;
    let outside = tool(f).map_or(&[][..], |t| t.outside);
    let (mut out, mut cursor, mut aged) = (Vec::new(), after, false);
    for mut e in events {
        cursor = e["seq"].as_i64().unwrap_or(cursor);
        if floor.is_some_and(|fl| e["at"].as_i64().unwrap_or(0) < fl) {
            aged = true;
            continue;
        }
        if !filter.keeps(&e) {
            continue;
        }
        if ctx.lockdown {
            lockdown::withhold(&mut e);
            lockdown::withhold_fields(&mut e, outside);
        }
        let at = e["at"].as_i64().unwrap_or(0);
        out.push(json!({ "jsonrpc": "2.0", "method": "notifications/events/event", "params": {
            "eventId": format!("{}:{cursor}", f.name), "name": f.name, "timestamp": iso8601(at), "data": e, "cursor": format!("{CURSOR}{cursor}"), "_meta": meta,
        } }));
    }
    if !more {
        cursor = page["next"].as_i64().unwrap_or(cursor).max(cursor);
    }
    Ok(Read { out, cursor, more, aged })
}

/// `events/stream` (push mode): one subscription's events on a stream of its own, while the
/// client holds it. Checked before it opens (an error is the answer, no stream); then
/// `notifications/events/active` (its cursor and heartbeat), each event as it's read (every 15
/// seconds, as the caller: its token's rules decide each read, so a revoked one ends the stream with
/// `notifications/events/terminated`), a heartbeat with the cursor when nothing else went for its
/// interval, `notifications/events/error` when a read failed for a moment, and its result when roll
/// closes it after an hour (the client reconnects with its cursor).
pub(crate) async fn stream(ctx: Arc<Ctx>, msg: &Value) -> axum::response::Response {
    use axum::response::IntoResponse;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let params = msg["params"].clone();
    let fail = |(code, message, data): Refused| axum::Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message, "data": data } })).into_response();
    let name = params["name"].as_str().unwrap_or("");
    let Some(f) = feed(name) else {
        return fail((-32023, format!("no event {name:?}: events/list says which there are"), json!({ "kind": "event" })));
    };
    if !usable(&ctx, f) {
        return fail((-32024, "Forbidden".into(), json!({ "reason": format!("this connection can't use {}", f.tool) })));
    }
    let Opened { filter, after: route, latest } = match open(f, &params["arguments"]) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let heartbeat = match &params["heartbeatMs"] {
        Value::Null => 30_000,
        v => match v.as_u64() {
            Some(n) => n.clamp(HEARTBEAT_MIN_MS, HEARTBEAT_MAX_MS),
            None => return fail(invalid("heartbeatMs is a number of milliseconds")),
        },
    };
    let floor = match &params["maxAgeMs"] {
        Value::Null => None,
        v => match v.as_i64().filter(|n| *n >= 0) {
            Some(n) => Some(crate::code_sql_now() - n),
            None => return fail(invalid("maxAgeMs is a number of milliseconds")),
        },
    };
    let who = match &ctx.principal {
        crate::Principal::Grant(g) => g.id.to_string(),
        crate::Principal::Operator => "operator".to_string(),
    };
    let Some(slot) = crate::ListenSlot::take(&format!("events:{who}"), STREAMS_PER_TOKEN) else {
        return fail((-32025, "ResourceExhausted".into(), json!({ "limit": "streams", "max": STREAMS_PER_TOKEN })));
    };
    let meta = json!({ "io.modelcontextprotocol/subscriptionId": id });
    // Where it starts: now (no cursor; `maxAgeMs` doesn't apply), the client's cursor, or now with a
    // possible gap for one roll can't read. The first read happens before the stream opens, so a
    // refusal is the answer and a gap is said on `active`.
    let (start, mut truncated, floor) = match &params["cursor"] {
        Value::String(c) if c.strip_prefix(CURSOR).is_some_and(|n| n.parse::<i64>().is_ok_and(|n| n >= 0)) => (c[CURSOR.len()..].parse::<i64>().ok(), false, floor),
        Value::String(_) => (None, true, None),
        _ => (None, false, None),
    };
    let (cursor, first) = match start {
        Some(after) => match read_feed(&ctx, f, &route, &filter, after, floor, &meta).await {
            Ok(r) => (r.cursor, Some(r)),
            Err(e) => return fail(e),
        },
        None => match crate::read_route_as(&ctx, &latest, &format!("events/stream:{}", f.name), json!({ "name": f.name }), true).await {
            Ok(page) => (page["next"].as_i64().unwrap_or(0), None),
            Err(e) => return fail(refused(f, e)),
        },
    };
    truncated |= first.as_ref().is_some_and(|r| r.aged);
    let mut queue = vec![json!({ "jsonrpc": "2.0", "method": "notifications/events/active", "params": { "cursor": format!("{CURSOR}{cursor}"), "truncated": truncated, "heartbeatMs": heartbeat, "_meta": meta } })];
    let more = first.as_ref().is_some_and(|r| r.more);
    queue.extend(first.map(|r| r.out).unwrap_or_default());
    struct State {
        ctx: Arc<Ctx>,
        filter: Filter,
        route: String,
        cursor: i64,
        queue: std::collections::VecDeque<Value>,
        more: bool,
        sent: tokio::time::Instant,
        done: bool,
        _slot: crate::ListenSlot,
    }
    let deadline = tokio::time::Instant::now() + STREAM_FOR;
    let state = State { ctx, filter, route, cursor, queue: queue.into(), more, sent: tokio::time::Instant::now(), done: false, _slot: slot };
    let beat = std::time::Duration::from_millis(heartbeat);
    let stream = futures::stream::unfold(state, move |mut s| {
        let (meta, id) = (meta.clone(), id.clone());
        async move {
            loop {
                if let Some(next) = s.queue.pop_front() {
                    s.sent = tokio::time::Instant::now();
                    return Some((next, s));
                }
                if s.done {
                    return None;
                }
                if tokio::time::Instant::now() >= deadline {
                    s.done = true;
                    s.queue.push_back(json!({ "jsonrpc": "2.0", "id": id, "result": { "resultType": "complete", "_meta": meta } }));
                    continue;
                }
                // A full page reads on at once; otherwise the next read waits its turn.
                if !s.more {
                    tokio::time::sleep(STREAM_READ_EVERY).await;
                }
                match read_feed(&s.ctx, f, &s.route, &s.filter, s.cursor, None, &meta).await {
                    Ok(r) => {
                        (s.cursor, s.more) = (r.cursor, r.more);
                        s.queue.extend(r.out);
                    }
                    // The connection can't read it any more (its token's rules, a policy), or the
                    // event type is gone: the subscription ends, and the stream with it.
                    Err((code @ (-32024 | -32023), message, data)) => {
                        s.done = true;
                        s.queue.push_back(json!({ "jsonrpc": "2.0", "method": "notifications/events/terminated", "params": { "error": { "code": code, "message": message, "data": data }, "_meta": meta } }));
                        s.queue.push_back(json!({ "jsonrpc": "2.0", "id": id, "result": { "resultType": "complete", "_meta": meta } }));
                        continue;
                    }
                    Err(_) => {
                        s.more = false;
                        s.queue.push_back(json!({ "jsonrpc": "2.0", "method": "notifications/events/error", "params": { "error": { "code": -32603, "message": "UpstreamError", "data": { "reason": "the feed didn't answer this time; roll reads it again" } }, "_meta": meta } }));
                        continue;
                    }
                }
                // Quiet past the next read: a heartbeat now, so the client's cursor moves.
                if s.queue.is_empty() && s.sent.elapsed() + STREAM_READ_EVERY > beat {
                    s.queue.push_back(json!({ "jsonrpc": "2.0", "method": "notifications/events/heartbeat", "params": { "cursor": format!("{CURSOR}{}", s.cursor), "_meta": meta } }));
                }
            }
        }
    })
    .map(|v| Ok::<_, std::convert::Infallible>(Event::default().event("message").data(v.to_string())));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(crate::SSE_KEEPALIVE)).into_response()
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
