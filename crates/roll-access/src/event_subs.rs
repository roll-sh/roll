//! MCP Events' webhook subscriptions (SEP-3415; docs/mcp.md, "Events"): what roll keeps for an
//! agent connection that asked to be POSTed what happens. A subscription is its connection's (a
//! token's or an OAuth connection's grant), keyed by that grant, the callback URL, the event's name
//! and its arguments (canonical JSON), so asking again refreshes it: its id is derived from the key
//! and routes deliveries, it's no capability. The client's signing secret is sealed (rolld's
//! [`SinkTransport`], `ROLL_SECRET_KEY`) and never shown again; a new one on refresh replaces it,
//! the old one still signing for a grace window. A callback URL proves it wants deliveries once per
//! connection (a signed challenge it echoes) before any are sent: a token can't aim roll at
//! someone else's endpoint.
//!
//! Deliveries are signed as Standard Webhooks (`webhook-id`, `webhook-timestamp`,
//! `webhook-signature: v1,<base64 HMAC-SHA256>` over `id.timestamp.body`, the key the secret's
//! base64 after `whsec_`), as the SEP says, not as roll's own webhooks are.

use crate::sinks::SinkTransport;
use crate::{Access, Error, Result};
use code_sql::{OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mcp_event_subs (
    id TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    arguments TEXT NOT NULL,
    url TEXT NOT NULL,
    secret TEXT NOT NULL,
    old_secret TEXT,
    old_until INTEGER,
    cursor TEXT NOT NULL,
    lockdown INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_delivery_at INTEGER,
    last_error TEXT,
    failed_since INTEGER,
    running_until INTEGER NOT NULL DEFAULT 0,
    next_at INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS mcp_event_subs_grant ON mcp_event_subs (grant_id);
CREATE INDEX IF NOT EXISTS mcp_event_subs_expires ON mcp_event_subs (expires_at);
CREATE TABLE IF NOT EXISTS mcp_event_receivers (
    grant_id TEXT NOT NULL,
    url TEXT NOT NULL,
    verified_at INTEGER NOT NULL,
    PRIMARY KEY (grant_id, url)
) WITHOUT ROWID;
";

/// Subscriptions one connection may hold.
pub const PER_GRANT: i64 = 20;
/// A subscription lives at most this long without a refresh (the client asks again before it ends).
pub const MAX_TTL_MS: i64 = 3_600_000;
/// …and at least this long (a client asking for less refreshes too often).
pub const MIN_TTL_MS: i64 = 60_000;
/// How long an old secret still signs after a new one replaces it.
const ROTATION_GRACE_MS: i64 = 5 * 60_000;
/// How often a subscription's feed is read (each read is its grant's request, in the audit log).
pub const READ_EVERY_MS: i64 = 60_000;
/// How long a claim holds a subscription for one node's delivery.
const HOLD_MS: i64 = 2 * 60_000;
/// After a delivery fails, the wait before trying again; failing this long, it's dropped.
const RETRY_MS: i64 = 5 * 60_000;
const GIVE_UP_MS: i64 = 24 * 3_600_000;
/// A callback's proof lasts a day: one whose domain now points elsewhere proves it again.
const VERIFIED_FOR_MS: i64 = 24 * 3_600_000;
/// Callback URLs one connection may ask roll to verify a minute: each is one POST to an address it
/// chose, so a token can't make roll knock on many.
const VERIFIES_PER_MINUTE: usize = 5;

/// A subscription as its connection sees it (never its secret).
#[derive(Clone, Debug)]
pub struct EventSub {
    pub id: String,
    pub grant_id: String,
    pub name: String,
    pub arguments: Value,
    pub url: String,
    pub cursor: String,
    pub lockdown: bool,
    pub expires_at: i64,
    pub last_delivery_at: Option<i64>,
    /// A category only (`timeout`, `http_5xx`…), never what the endpoint answered.
    pub last_error: Option<String>,
    pub failed_since: Option<i64>,
}

/// What a subscribe asks for.
pub struct NewEventSub<'a> {
    pub grant_id: &'a str,
    pub name: &'a str,
    pub arguments: &'a Value,
    pub url: &'a str,
    pub secret: &'a str,
    /// Where deliveries start when it's new (an existing one keeps its own).
    pub cursor: &'a str,
    pub lockdown: bool,
    pub ttl_ms: i64,
}

/// Why a callback endpoint didn't take a delivery: the SEP's categories, nothing it said.
pub fn endpoint_error(transport_error: &str) -> &'static str {
    match transport_error {
        "timeout" => "timeout",
        "tls_error" => "tls_error",
        e if e.starts_with("http_4") => "http_4xx",
        e if e.starts_with("http_5") => "http_5xx",
        _ => "connection_refused",
    }
}

/// The subscription key's id: `sub_` and 20 hex characters of the key's SHA-256.
pub fn sub_id(grant_id: &str, url: &str, name: &str, arguments: &Value) -> String {
    use sha2::Digest;
    let key = format!("{grant_id}\0{url}\0{name}\0{}", canonical(arguments));
    let h = sha2::Sha256::digest(key.as_bytes());
    format!("sub_{}", h.iter().take(10).map(|b| format!("{b:02x}")).collect::<String>())
}

/// Arguments as canonical JSON (keys sorted, as serde_json's map keeps them).
pub fn canonical(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// The signing key a `whsec_` secret holds: the base64 of 24 to 64 bytes (the SEP's rule).
pub fn secret_key(secret: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let b64 = secret.strip_prefix("whsec_")?;
    let e = base64::engine::general_purpose::STANDARD;
    let key = e.decode(b64).ok().or_else(|| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64).ok())?;
    (24..=64).contains(&key.len()).then_some(key)
}

/// A Standard Webhooks signature (`v1,<base64>`) of a message.
pub fn sign(key: &[u8], msg_id: &str, timestamp: i64, body: &[u8]) -> String {
    use base64::Engine;
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(format!("{msg_id}.{timestamp}.").as_bytes());
    mac.update(body);
    format!("v1,{}", base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// A message id: high entropy, never derived from the event (`msg_<kind>_<random>`).
pub fn msg_id(kind: &str) -> String {
    format!("msg_{kind}_{}", crate::random(22))
}

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<EventSub> {
    let args: String = r.get(3)?;
    Ok(EventSub {
        id: r.get(0)?,
        grant_id: r.get(1)?,
        name: r.get(2)?,
        arguments: serde_json::from_str(&args).unwrap_or(Value::Null),
        url: r.get(4)?,
        cursor: r.get(5)?,
        lockdown: r.get::<_, i64>(6)? != 0,
        expires_at: r.get(7)?,
        last_delivery_at: r.get(8)?,
        last_error: r.get(9)?,
        failed_since: r.get(10)?,
    })
}

const COLUMNS: &str = "id, grant_id, name, arguments, url, cursor, lockdown, expires_at, last_delivery_at, last_error, failed_since";

impl Access {
    fn event_transport(&self) -> Result<Arc<dyn SinkTransport>> {
        self.event_hooks.get().cloned().ok_or_else(|| Error::BadRequest("webhook delivery isn't set up on this server".into()))
    }

    /// Whether `url` may be a callback (https and public; what rolld's transport for them allows).
    pub fn event_url_ok(&self, url: &str) -> Result<()> {
        self.event_transport()?.check(url).map_err(|_| Error::BadRequest("delivery.url is https on a public host".into()))
    }

    /// Whether this grant may have webhook deliveries: not one held to some addresses (a delivery
    /// goes to an address its list doesn't name, and roll's reads for it come from none).
    pub fn event_subscriber_ok(&self, grant_id: &str) -> Result<()> {
        let g = self.store.get(grant_id)?;
        if g.ip_allow.is_empty() { Ok(()) } else { Err(Error::Forbidden("a connection held to some addresses gets no webhook deliveries: poll instead".into())) }
    }

    /// How many live subscriptions this grant holds.
    pub fn event_subs_held(&self, grant_id: &str) -> Result<i64> {
        let now = code_sql::now_ms();
        Ok(self.store.db.query("SELECT COUNT(*) FROM mcp_event_subs WHERE grant_id = ? AND expires_at > ?", params![grant_id, now], |r| r.get(0))?.into_iter().next().unwrap_or(0))
    }

    /// The subscription a key names, if it's live.
    pub fn event_sub(&self, grant_id: &str, url: &str, name: &str, arguments: &Value) -> Result<Option<EventSub>> {
        let id = sub_id(grant_id, url, name, arguments);
        let now = code_sql::now_ms();
        Ok(self.store.db.query(&format!("SELECT {COLUMNS} FROM mcp_event_subs WHERE id = ? AND expires_at > ?"), params![id, now], row)?.into_iter().next())
    }

    /// Make or refresh a subscription in one write: a new one counts against its connection's 20
    /// and starts at `cursor`; an existing one keeps its own cursor, takes the new TTL and secret
    /// (the old one signing a while longer). Expired ones are dropped as it goes.
    pub fn put_event_sub(&self, n: &NewEventSub<'_>) -> Result<EventSub> {
        let id = sub_id(n.grant_id, n.url, n.name, n.arguments);
        let t = self.event_transport()?;
        let sealed = t.seal(&id, n.secret).map_err(Error::Internal)?;
        let now = code_sql::now_ms();
        let expires = now + n.ttl_ms.clamp(MIN_TTL_MS, MAX_TTL_MS);
        let args = canonical(n.arguments);
        self.store.db.write(|c| {
            c.execute("DELETE FROM mcp_event_subs WHERE expires_at <= ?", [now])?;
            let had: Option<String> = c.query_row("SELECT secret FROM mcp_event_subs WHERE id = ?", [&id], |r| r.get(0)).optional()?;
            match had {
                Some(old) => {
                    let rotated = t.open(&id, &old).map(|o| o != n.secret).unwrap_or(true);
                    if rotated {
                        c.execute("UPDATE mcp_event_subs SET old_secret = secret, old_until = ?, secret = ?, expires_at = ?, lockdown = ? WHERE id = ? AND grant_id = ?", params![now + ROTATION_GRACE_MS, sealed, expires, n.lockdown, id, n.grant_id])?;
                    } else {
                        c.execute("UPDATE mcp_event_subs SET expires_at = ?, lockdown = ? WHERE id = ? AND grant_id = ?", params![expires, n.lockdown, id, n.grant_id])?;
                    }
                }
                None => {
                    let held: i64 = c.query_row("SELECT COUNT(*) FROM mcp_event_subs WHERE grant_id = ?", [n.grant_id], |r| r.get(0))?;
                    if held >= PER_GRANT {
                        return Err(Error::TooMany(format!("a connection holds at most {PER_GRANT} event subscriptions")));
                    }
                    c.execute(
                        "INSERT INTO mcp_event_subs (id, grant_id, name, arguments, url, secret, cursor, lockdown, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        params![id, n.grant_id, n.name, args, n.url, sealed, n.cursor, n.lockdown, expires, now],
                    )?;
                }
            }
            Ok::<_, Error>(())
        })?;
        self.event_sub(n.grant_id, n.url, n.name, n.arguments)?.ok_or_else(|| Error::Internal("the subscription went as it was made".into()))
    }

    /// End a subscription; whether there was one.
    pub fn delete_event_sub(&self, grant_id: &str, url: &str, name: &str, arguments: &Value) -> Result<bool> {
        let id = sub_id(grant_id, url, name, arguments);
        self.store.db.write(|c| Ok::<_, Error>(c.execute("DELETE FROM mcp_event_subs WHERE id = ? AND grant_id = ?", params![id, grant_id])? > 0))
    }

    /// A subscription made, refreshed or ended, in the audit log as its connection's (the callback
    /// by its host only: a path or query may hold the receiver's token).
    pub fn audit_event_sub(&self, grant_id: &str, what: &str, name: &str, url: &str, decision: &str) {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let e = crate::store::AuditEvent {
            id: crate::new_id("aud"),
            at: code_sql::now_ms(),
            grant_id: Some(grant_id.to_string()),
            grant_name: self.store.get(grant_id).ok().map(|g| g.name),
            source: "mcp".into(),
            tool: Some(format!("events/{what}:{name}")),
            method: "POST".into(),
            route: "/v1/mcp".into(),
            product: Some("access".into()),
            action: Some("write".into()),
            resource: None,
            decision: decision.into(),
            approval: None,
            status: 200,
            args: Some(json!({ "name": name, "callback": host })),
            ip: None,
            duration_ms: 0,
            client: None,
        };
        if let Some(e) = self.store.queue_audit(e) {
            self.store.audit(&e);
        }
    }

    /// Whether this connection's callback URL proved it wants deliveries.
    pub fn event_receiver_verified(&self, grant_id: &str, url: &str) -> Result<bool> {
        let since = code_sql::now_ms() - VERIFIED_FOR_MS;
        Ok(self.store.db.query("SELECT 1 FROM mcp_event_receivers WHERE grant_id = ? AND url = ? AND verified_at > ?", params![grant_id, url, since], |_| Ok(()))?.into_iter().next().is_some())
    }

    /// Ask `url` to prove it wants this connection's deliveries: a signed
    /// `{"type":"verification","challenge"}` it must echo in a 2xx answer. `Err` is the SEP's
    /// category (`challenge_failed`, `timeout`, `http_4xx`…), never what the endpoint said.
    pub async fn verify_event_receiver(self: &Arc<Self>, grant_id: &str, url: &str, secret: &str, sub_id: &str) -> std::result::Result<(), String> {
        if self.event_verifies.lock().unwrap_or_else(|e| e.into_inner()).admit(grant_id, code_sql::now_ms(), VERIFIES_PER_MINUTE).is_err() {
            return Err("rate".into());
        }
        let key = secret_key(secret).ok_or("challenge_failed")?;
        let t = self.event_transport().map_err(|_| "connection_refused".to_string())?;
        let challenge = crate::random(32);
        let body = serde_json::to_vec(&json!({ "type": "verification", "challenge": challenge })).unwrap_or_default();
        let (id, ts) = (msg_id("verification"), code_sql::now_ms() / 1000);
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("webhook-id".to_string(), id.clone()),
            ("webhook-timestamp".to_string(), ts.to_string()),
            ("webhook-signature".to_string(), sign(&key, &id, ts, &body)),
            ("x-mcp-subscription-id".to_string(), sub_id.to_string()),
        ];
        let (status, answer) = t.post_answer(url.to_string(), headers, body).await.map_err(|e| endpoint_error(&e).to_string())?;
        if !(200..300).contains(&status) {
            return Err(endpoint_error(&format!("http_{status}")).into());
        }
        let echoed = serde_json::from_slice::<Value>(&answer).ok().and_then(|v| v["challenge"].as_str().map(str::to_string)).unwrap_or_default();
        if !roll_auth::constant_eq(echoed.as_bytes(), challenge.as_bytes()) {
            return Err("challenge_failed".into());
        }
        let (g, u, now) = (grant_id.to_string(), url.to_string(), code_sql::now_ms());
        self.store
            .db
            .write(|c| Ok::<_, Error>(c.execute("INSERT INTO mcp_event_receivers (grant_id, url, verified_at) VALUES (?, ?, ?) ON CONFLICT (grant_id, url) DO UPDATE SET verified_at = excluded.verified_at", params![g, u, now])?))
            .map_err(|_| "connection_refused".to_string())?;
        Ok(())
    }
}

/// A subscription claimed for delivery, with the keys its deliveries are signed with (the current
/// secret's, and an old one's during its grace: both signatures go, either verifies).
pub struct Claimed {
    pub sub: EventSub,
    pub keys: Vec<Vec<u8>>,
    /// The claim's hold: what this round writes back only while it's still the one holding it.
    pub claim: i64,
}

impl Access {
    /// Claim up to `limit` subscriptions to these events (the ones whose product runs here) due a
    /// read, in one write, so no two nodes deliver one.
    pub fn claim_event_subs(&self, limit: i64, names: &[&str]) -> Result<Vec<Claimed>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let t = self.event_transport()?;
        let now = code_sql::now_ms();
        let claim = now + HOLD_MS;
        let marks = vec!["?"; names.len()].join(", ");
        let rows: Vec<(EventSub, String, Option<String>, Option<i64>)> = self.store.db.write(|c| {
            c.execute("DELETE FROM mcp_event_subs WHERE expires_at <= ?", [now])?;
            let mut args: Vec<code_sql::Value> = vec![code_sql::Value::Int(now), code_sql::Value::Int(now)];
            args.extend(names.iter().map(|n| code_sql::Value::Text(n.to_string())));
            args.push(code_sql::Value::Int(limit));
            let due = c.query(
                &format!("SELECT {COLUMNS}, secret, old_secret, old_until FROM mcp_event_subs WHERE running_until < ? AND next_at <= ? AND name IN ({marks}) ORDER BY next_at LIMIT ?"),
                args.as_slice(),
                |r| Ok((row(r)?, r.get::<_, String>(11)?, r.get::<_, Option<String>>(12)?, r.get::<_, Option<i64>>(13)?)),
            )?;
            for (s, ..) in &due {
                c.execute("UPDATE mcp_event_subs SET running_until = ? WHERE id = ?", params![claim, s.id])?;
            }
            Ok::<_, Error>(due)
        })?;
        Ok(rows
            .into_iter()
            .map(|(sub, secret, old, until)| {
                let mut keys: Vec<Vec<u8>> = t.open(&sub.id, &secret).ok().and_then(|s| secret_key(&s)).into_iter().collect();
                if let (Some(o), Some(u)) = (old, until)
                    && u > now
                    && let Some(k) = t.open(&sub.id, &o).ok().and_then(|s| secret_key(&s))
                {
                    keys.push(k);
                }
                Claimed { sub, keys, claim }
            })
            .collect())
    }

    /// A claimed subscription's round: where it got to, and whether a delivery failed (`error`, a
    /// category). A success clears the failure; one failing a day is dropped.
    /// A claimed subscription's round: where it got to, and whether a delivery failed (`error`, a
    /// category). A success clears the failure; one failing a day is dropped. Written only while
    /// this round still holds the claim (a slower node's round never moves a cursor back).
    pub fn event_sub_done(&self, c: &Claimed, cursor: &str, delivered: bool, error: Option<&str>) -> Result<()> {
        let (id, claim, now) = (&c.sub.id, c.claim, code_sql::now_ms());
        self.store.db.write(|c| {
            match error {
                None => c.execute(
                    "UPDATE mcp_event_subs SET cursor = ?, running_until = 0, next_at = ?, last_error = NULL, failed_since = NULL, last_delivery_at = CASE WHEN ? THEN ? ELSE last_delivery_at END WHERE id = ? AND running_until = ?",
                    params![cursor, now + READ_EVERY_MS, delivered, now, id, claim],
                )?,
                Some(e) => {
                    c.execute("DELETE FROM mcp_event_subs WHERE id = ? AND running_until = ? AND failed_since IS NOT NULL AND failed_since < ?", params![id, claim, now - GIVE_UP_MS])?;
                    c.execute(
                        "UPDATE mcp_event_subs SET cursor = ?, running_until = 0, next_at = ?, last_error = ?, failed_since = COALESCE(failed_since, ?), last_delivery_at = CASE WHEN ? THEN ? ELSE last_delivery_at END WHERE id = ? AND running_until = ?",
                        params![cursor, now + RETRY_MS, e, now, delivered, now, id, claim],
                    )?
                }
            };
            Ok::<_, Error>(())
        })
    }

    /// A claimed subscription read nothing this round (its feed's product was busy): the next
    /// read waits as usual, its delivery state as it was.
    pub fn event_sub_retry(&self, c: &Claimed) -> Result<()> {
        let (id, claim, next) = (&c.sub.id, c.claim, code_sql::now_ms() + READ_EVERY_MS);
        self.store.db.write(|c| Ok::<_, Error>(c.execute("UPDATE mcp_event_subs SET running_until = 0, next_at = ? WHERE id = ? AND running_until = ?", params![next, id, claim]).map(|_| ())?))
    }

    /// End a claimed subscription roll can't deliver any more (its grant's access ended, its
    /// endpoint said it's gone).
    pub fn drop_event_sub(&self, c: &Claimed) -> Result<()> {
        let (id, claim) = (&c.sub.id, c.claim);
        self.store.db.write(|c| Ok::<_, Error>(c.execute("DELETE FROM mcp_event_subs WHERE id = ? AND running_until = ?", params![id, claim]).map(|_| ())?))
    }

    /// POST a signed message to a subscription's callback: `Ok(status)` for any answer, `Err` a
    /// category when there was none.
    pub async fn post_event(&self, c: &Claimed, msg_id: &str, body: Vec<u8>) -> std::result::Result<u16, String> {
        let t = self.event_transport().map_err(|_| "connection_refused".to_string())?;
        let ts = code_sql::now_ms() / 1000;
        let sigs: Vec<String> = c.keys.iter().map(|k| sign(k, msg_id, ts, &body)).collect();
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("webhook-id".to_string(), msg_id.to_string()),
            ("webhook-timestamp".to_string(), ts.to_string()),
            ("webhook-signature".to_string(), sigs.join(" ")),
            ("x-mcp-subscription-id".to_string(), c.sub.id.clone()),
        ];
        t.post_answer(c.sub.url.clone(), headers, body).await.map(|(s, _)| s).map_err(|e| endpoint_error(&e).to_string())
    }
}

/// One event's message id on one subscription: the same on every retry of it, a new secret too (a
/// hash of the subscription and the event's position, so a receiver drops repeats), unique across
/// them.
pub fn event_msg_id(sub_id: &str, seq: i64) -> String {
    use base64::Engine;
    use sha2::Digest;
    let mut mac = sha2::Sha256::new();
    mac.update(format!("roll-mcp-event\0{sub_id}\0{seq}").as_bytes());
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize());
    format!("msg_event_{}", &b[..22])
}

#[cfg(test)]
mod tests {
    /// The Standard Webhooks test vector (standard-webhooks/standard-webhooks, the spec's example).
    #[test]
    fn signatures_are_standard_webhooks() {
        let key = super::secret_key("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        let sig = super::sign(&key, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, br#"{"test": 2432232314}"#);
        assert_eq!(sig, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
        assert!(super::secret_key("whsec_c2hvcnQ=").is_none(), "under 24 bytes");
        assert!(super::secret_key("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").is_none(), "no prefix");
    }
}
