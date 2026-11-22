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
    let hooks = ctx.m.config.access.as_ref().is_some_and(|a| a.sinks.get().is_some()) && subscriber(ctx).is_ok();
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
        let page = crate::read_route_as(ctx, f.latest, &what, audit).await.map_err(|e| refused(f, e))?;
        let next = page["next"].as_i64().unwrap_or(0);
        return Ok(json!({ "events": [], "cursor": format!("{CURSOR}{next}"), "truncated": truncated, "hasMore": false, "nextPollMs": POLL_MS }));
    };
    let page = crate::read_route_as(ctx, &format!("{}{after}", f.after), &what, audit).await.map_err(|e| refused(f, e))?;
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
fn subscriber(ctx: &Ctx) -> Result<(&roll_access::Access, String), Refused> {
    let forbidden = |why: &str| (-32024, "Forbidden".to_string(), json!({ "reason": why }));
    let access = ctx.m.config.access.as_deref().ok_or_else(|| (-32026, "Unsupported".to_string(), json!({ "feature": "deliveryMode", "value": "webhook" })))?;
    match &ctx.principal {
        crate::Principal::Grant(g) if matches!(g.kind, roll_auth::GrantKind::Token | roll_auth::GrantKind::Oauth) => Ok((access, g.id.to_string())),
        _ => Err(forbidden("webhook subscriptions are a token's or an OAuth connection's: connect with one")),
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
/// client's secret, until the subscription isn't refreshed. A callback proves once per connection
/// that it wants deliveries (it echoes a signed challenge) before the subscription is kept.
pub(crate) async fn subscribe(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, Refused> {
    let (access, grant) = subscriber(ctx)?;
    let (f, args, url) = keyed(ctx, params)?;
    match params["delivery"]["mode"].as_str() {
        Some("webhook") => {}
        Some(m) => return Err((-32026, "Unsupported".into(), json!({ "feature": "deliveryMode", "value": m }))),
        None => return Err(invalid("delivery.mode is webhook")),
    }
    // https and a public host (the SEP's rule and roll's guard); a dev server may allow http.
    access.event_url_ok(url).map_err(|e| invalid(format!("delivery.url: {e}")))?;
    let secret = params["delivery"]["secret"].as_str().unwrap_or("");
    if roll_access::event_subs::secret_key(secret).is_none() {
        return Err(invalid("delivery.secret is whsec_ and the base64 of 24 to 64 random bytes"));
    }
    let ttl = match &params["ttlMs"] {
        Value::Null => roll_access::event_subs::MAX_TTL_MS,
        v => v.as_i64().filter(|n| *n > 0).ok_or_else(|| invalid("ttlMs is a number of milliseconds"))?,
    };
    let existing = access.event_sub(&grant, url, f.name, &args).map_err(internal)?;
    // A new one starts where the client says, or now; a refresh keeps the cursor roll delivered to.
    let (cursor, truncated) = match (&existing, &params["cursor"]) {
        (Some(e), _) => (e.cursor.clone(), false),
        (None, Value::String(c)) if c.strip_prefix(CURSOR).is_some_and(|n| n.parse::<i64>().is_ok_and(|n| n >= 0)) => (c.clone(), false),
        (None, c) => {
            let page = crate::read_route_as(ctx, f.latest, &format!("events/subscribe:{}", f.name), json!({ "name": f.name, "arguments": args })).await.map_err(|e| refused(f, e))?;
            (format!("{CURSOR}{}", page["next"].as_i64().unwrap_or(0)), c.is_string())
        }
    };
    let id = roll_access::event_subs::sub_id(&grant, url, f.name, &args);
    if !access.event_receiver_verified(&grant, url).map_err(internal)? {
        let a = ctx.m.config.access.clone().expect("checked above");
        if let Err(reason) = a.verify_event_receiver(&grant, url, secret, &id).await {
            if reason == "rate" {
                return Err((-32025, "ResourceExhausted".into(), json!({ "limit": "verifications" })));
            }
            return Err((-32027, "CallbackEndpointError".into(), json!({ "reason": reason })));
        }
    }
    let new = roll_access::event_subs::NewEventSub { grant_id: &grant, name: f.name, arguments: &args, url, secret, cursor: &cursor, lockdown: ctx.lockdown, ttl_ms: ttl };
    let sub = access.put_event_sub(&new).map_err(|e| match e {
        roll_access::Error::TooMany(m) => (-32025, m, json!({ "limit": "subscriptions", "max": roll_access::event_subs::PER_GRANT })),
        e => internal(e),
    })?;
    access.audit_event_sub(&grant, "subscribe", f.name, url, if existing.is_some() { "refresh" } else { "allow" });
    let mut out = json!({ "id": sub.id, "refreshBefore": iso8601(sub.expires_at), "cursor": sub.cursor, "truncated": truncated });
    if existing.is_some() {
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
    let url = params["delivery"]["url"].as_str().ok_or_else(|| invalid("delivery.url names the subscription"))?;
    if access.delete_event_sub(&grant, url, f.name, &args).map_err(internal)? {
        access.audit_event_sub(&grant, "unsubscribe", f.name, url, "allow");
        Ok(json!({}))
    } else {
        Err((-32023, "no such subscription of this connection's".into(), json!({ "kind": "subscription" })))
    }
}

fn internal(e: roll_access::Error) -> Refused {
    (-32603, e.to_string(), json!({}))
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
