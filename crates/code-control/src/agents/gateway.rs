//! The owner's model gateway on the relay (docs/code/gateway.md): the plan ([`GatewayPlan`]: which
//! machine hosts it, the URLs members reach it by, which machines use it), and joining and leaving.
//! The relay never calls the gateway and never sees a key:
//!
//! - **Join** (`POST …/gateway/members`): the relay asks the host for a client key sealed to the
//!   member's seal key (`gateway.client.issue`; the member's `seal_key` comes from its mirrored
//!   `hello`), and passes the box on unchanged in the member's `endpoint.set` (id `roll-gateway`,
//!   kind `gateway`), which the member's daemon opens as an ordinary endpoint key. Only ciphertext
//!   crosses the relay, in memory, while forwarding; the plan records the client's id or why
//!   joining failed.
//! - **Leave** (`DELETE …/gateway/members/{machine}`): `endpoint.remove` on the member (best effort:
//!   it may be gone) and the client revoked on the host (that has to work, or the member stays
//!   with the error, for the owner to try again).
//! - **Sandboxes**: with `new_sandboxes`, a sandbox's first `hello` joins it ([`sandbox_hello`],
//!   spawned from the link, which runs on the node holding the owner's `agents:` lease).
//!
//! Every route is the owner's (`Code::manages`): agent tokens get 403. Commands go through
//! [`Code::machine_command`] (a sleeping sandbox is woken) as the owner, or as `roll` for a sandbox's
//! automatic join.

use super::api::{live_machine, manageable, sender};
use super::store::{self, MachineRow};
use crate::api::Body;
use crate::{Caller, Code, Error, Result};
use agent_proto::{
    Command, EndpointKind, EndpointSpec, ErrorCode, GatewayJoin, GatewayMember, GatewayPlan, KeyChange, MachineId, ProviderId, Reply, gateway_limits,
};
use axum::extract::{Path, State};
use axum::Json;
use code_sql::{OptionalExtension, params};
use std::sync::Arc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_gateway_plans (
    owner_id TEXT PRIMARY KEY,
    -- The plan, JSON (`agent_proto::GatewayPlan`): machines, URLs and client ids; never a key.
    json TEXT NOT NULL,
    -- Unix ms.
    updated_at INTEGER NOT NULL
);
"#;

/// Who a sandbox's automatic join is sent as.
const RELAY: &str = "roll";

pub fn plan(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Option<GatewayPlan>> {
    let json: Option<String> = db.query_row("SELECT json FROM agent_gateway_plans WHERE owner_id = ?", [owner_id], |r| r.get(0)).optional()?;
    Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
}

fn plan_on(c: &code_sql::Conn<'_>, owner_id: &str) -> code_sql::Result<Option<GatewayPlan>> {
    let json: Option<String> = c.query_row("SELECT json FROM agent_gateway_plans WHERE owner_id = ?", [owner_id], |r| r.get(0)).optional()?;
    Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
}

fn save_on(c: &code_sql::Conn<'_>, owner_id: &str, plan: &GatewayPlan) -> Result<()> {
    let json = serde_json::to_string(plan).map_err(|e| Error::Internal(e.into()))?;
    c.execute(
        "INSERT INTO agent_gateway_plans (owner_id, json, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT (owner_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
        params![owner_id, json, plan.updated_at as i64],
    )?;
    Ok(())
}

fn now() -> u64 {
    code_sql::now_ms().max(0) as u64
}

fn no_plan() -> Error {
    Error::NotFound("the gateway (set it up first)".into())
}

/// `GET …/gateway`: the plan, or `null` while none is set up (no gateway is a state, not an error).
pub(super) async fn get_plan(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>) -> Result<Json<Option<GatewayPlan>>> {
    let o = manageable(&code, &caller, &owner)?;
    Ok(Json(plan(code.db.sql(), &o.id)?))
}

/// `PUT …/gateway GatewayPlan`: the host, URLs, `new_sandboxes`, `fallback` and when the owner
/// acknowledged the notice. Members are the relay's (joining and leaving); any sent are ignored.
/// Another host while machines use the gateway is refused.
pub(super) async fn put_plan(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<GatewayPlan>> {
    let o = manageable(&code, &caller, &owner)?;
    let mut wanted: GatewayPlan = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid gateway: {e}")))?;
    wanted.members.clear();
    let mut wanted = wanted.check().map_err(Error::Invalid)?;
    if wanted.acknowledged_at == 0 {
        return Err(Error::Invalid("read and acknowledge the notice first: every AI request from the machines that use the gateway goes out from its host".into()));
    }
    live_machine(&code, &o, &wanted.host.0)?;
    wanted.updated_at = now();
    let saved = code.db.sql().write(|c| -> Result<GatewayPlan> {
        let mut next = wanted.clone();
        if let Some(old) = plan_on(c, &o.id)? {
            if old.host != next.host && !old.members.is_empty() {
                return Err(Error::Conflict("machines use the gateway on its current host: remove the machines first".into()));
            }
            next.members = old.members;
        }
        save_on(c, &o.id, &next)?;
        Ok(next)
    })?;
    Ok(Json(saved))
}

/// `DELETE …/gateway`: forget the plan (the host's gateway is removed with `gateway.remove`).
/// Refused while machines use it.
pub(super) async fn delete_plan(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>) -> Result<axum::http::StatusCode> {
    let o = manageable(&code, &caller, &owner)?;
    code.db.sql().write(|c| -> Result<()> {
        let Some(old) = plan_on(c, &o.id)? else { return Err(no_plan()) };
        if !old.members.is_empty() {
            return Err(Error::Conflict("machines still use the gateway: remove the machines first".into()));
        }
        c.execute("DELETE FROM agent_gateway_plans WHERE owner_id = ?", [&o.id])?;
        Ok(())
    })?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST …/gateway/members GatewayJoin` → the plan, once the member's endpoint is set. A failure
/// is recorded on the member (`error`) and answered with the daemon's status.
pub(super) async fn join(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<GatewayPlan>> {
    let o = manageable(&code, &caller, &owner)?;
    let want: GatewayJoin = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid join: {e}")))?;
    let member = live_machine(&code, &o, &want.machine.0)?;
    Ok(Json(join_machine(&code, &o.id, &member, want.url, want.providers, &sender(&caller)).await?))
}

/// Joins `member` to the owner's gateway (see the module doc).
pub(crate) async fn join_machine(code: &Code, owner_id: &str, member: &MachineRow, url: Option<String>, providers: Vec<ProviderId>, by: &str) -> Result<GatewayPlan> {
    let db = code.db.sql();
    let p = plan(db, owner_id)?.ok_or_else(no_plan)?;
    // A gateway's URLs are the owner's network (Tailscale, a LAN, Docker's bridge): a sandbox roll
    // hosts elsewhere can't reach them (docs/code/gateway.md, "Sandboxes").
    if let Some(s) = member.sandbox.as_ref().filter(|s| s.backend != "docker") {
        return Err(Error::Conflict(format!("{} is a sandbox hosted by roll ({}): it can't reach your computers, so it can't use the gateway; give it its own keys or login", member.name, s.backend)));
    }
    let host = match store::machine(db, &p.host.0)? {
        Some(h) if h.owner_id == owner_id && !h.revoked => h,
        _ => return Err(Error::Conflict("the gateway's host is gone: set the gateway up on another machine".into())),
    };
    let url = match url.map(|u| u.trim().trim_end_matches('/').to_string()) {
        None => p.urls.first().map(|u| u.url.clone()).ok_or_else(|| Error::Invalid("the gateway has no URL for machines to reach it by: add one".into()))?,
        Some(u) if p.urls.iter().any(|known| known.url == u) => u,
        // The host reaches itself on loopback.
        Some(u) if member.id == host.id && is_loopback_url(&u) => u,
        Some(u) => return Err(Error::Invalid(format!("{u} isn't one of the gateway's URLs"))),
    };
    let Some(seal_key) = member.info.as_ref().and_then(|i| i.seal_key.clone()) else {
        return Err(Error::Conflict(format!("{} hasn't connected with a roll that seals keys yet: update roll there and start it", member.name)));
    };
    let providers = if providers.is_empty() {
        member.info.as_ref().map(|i| i.providers.iter().map(|p| p.id.clone()).collect()).unwrap_or_default()
    } else {
        providers
    };
    if providers.is_empty() {
        return Err(Error::Conflict(format!("{} has no providers to use the gateway yet", member.name)));
    }
    let suffix = super::api::random_hex(6);
    let issued = code
        .machine_command(
            &host,
            format!("gw-issue-{}-{suffix}", member.id),
            by.to_string(),
            Command::GatewayClientIssue { name: member.name.clone(), machine: Some(MachineId(member.id.clone())), seal_key },
            None,
        )
        .await;
    let issued = match issued {
        Reply::GatewayClient(i) => i,
        other => return Err(record_failure(code, owner_id, &member.id, &url, &providers, "the gateway's host", other)),
    };
    let spec = EndpointSpec {
        id: gateway_limits::ENDPOINT_ID.into(),
        name: format!("roll gateway on {}", host.name),
        kind: EndpointKind::Gateway,
        base_url: url.clone(),
        providers: providers.clone(),
        url_env: None,
        key_env: None,
        key_ref: None,
        fallback: p.fallback,
    };
    let set = code
        .machine_command(member, format!("gw-join-{}-{suffix}", member.id), by.to_string(), Command::EndpointSet { endpoint: spec, key: KeyChange::Set { sealed: issued.sealed } }, None)
        .await;
    if !matches!(set, Reply::Ok) {
        // Nobody holds the key: take it back.
        let revoke = Command::GatewayClientRevoke { client: issued.client.id.clone() };
        let _ = code.machine_command(&host, format!("gw-unissue-{}-{suffix}", member.id), by.to_string(), revoke, None).await;
        return Err(record_failure(code, owner_id, &member.id, &url, &providers, &member.name, set));
    }
    let entry = GatewayMember { machine: MachineId(member.id.clone()), url, providers, client: Some(issued.client.id), joined_at: now(), error: None };
    update_member(code, owner_id, entry)
}

/// Keeps `entry` as the plan's member for its machine (replacing an earlier one).
fn update_member(code: &Code, owner_id: &str, entry: GatewayMember) -> Result<GatewayPlan> {
    code.db.sql().write(|c| -> Result<GatewayPlan> {
        let Some(mut p) = plan_on(c, owner_id)? else { return Err(no_plan()) };
        let full = p.members.len() >= gateway_limits::CLIENTS;
        match p.members.iter_mut().find(|m| m.machine == entry.machine) {
            Some(m) => *m = entry.clone(),
            None if full => return Err(Error::Invalid(format!("at most {} machines use one gateway", gateway_limits::CLIENTS))),
            None => p.members.push(entry.clone()),
        }
        p.updated_at = now();
        save_on(c, owner_id, &p)?;
        Ok(p)
    })
}

/// Records why joining failed on the member (keeping a client it already had), and the error to
/// answer with.
fn record_failure(code: &Code, owner_id: &str, machine: &str, url: &str, providers: &[ProviderId], who: &str, reply: Reply) -> Error {
    let (code_, message) = match reply {
        Reply::Error(e) => (e.code, e.message),
        other => (ErrorCode::Internal, format!("unexpected answer: {}", serde_json::to_value(&other).map(|v| v["type"].to_string()).unwrap_or_default())),
    };
    let why = format!("{who}: {message}");
    let earlier = plan(code.db.sql(), owner_id).ok().flatten().and_then(|p| p.member(&MachineId(machine.to_string())).cloned());
    let entry = GatewayMember {
        machine: MachineId(machine.to_string()),
        url: earlier.as_ref().map_or_else(|| url.to_string(), |e| e.url.clone()),
        providers: earlier.as_ref().map_or_else(|| providers.to_vec(), |e| e.providers.clone()),
        client: earlier.as_ref().and_then(|e| e.client.clone()),
        joined_at: earlier.as_ref().map_or(0, |e| e.joined_at),
        error: Some(why.clone()),
    };
    if let Err(e) = update_member(code, owner_id, entry) {
        tracing::warn!(machine = %machine, error = %e, "recording a failed gateway join");
    }
    match code_ {
        ErrorCode::Invalid => Error::Invalid(why),
        ErrorCode::NotFound | ErrorCode::Conflict => Error::Conflict(why),
        ErrorCode::MachineOffline | ErrorCode::Timeout => Error::Unavailable(why),
        ErrorCode::Provider | ErrorCode::Internal => Error::Unprocessable(why),
    }
}

/// `DELETE …/gateway/members/{machine}` → the plan without it.
pub(super) async fn leave(State(code): State<Arc<Code>>, caller: Caller, Path((owner, machine)): Path<(String, String)>) -> Result<Json<GatewayPlan>> {
    let o = manageable(&code, &caller, &owner)?;
    let db = code.db.sql();
    let p = plan(db, &o.id)?.ok_or_else(no_plan)?;
    let Some(entry) = p.member(&MachineId(machine.clone())).cloned() else { return Err(Error::NotFound(format!("machine {machine} in the gateway"))) };
    let by = sender(&caller);
    let suffix = super::api::random_hex(6);
    // The member forgets the endpoint; a machine that's gone or offline can't, and its key is
    // revoked below anyway.
    if let Some(m) = store::machine(db, &machine)?.filter(|m| m.owner_id == o.id && !m.revoked) {
        let remove = Command::EndpointRemove { endpoint: gateway_limits::ENDPOINT_ID.into() };
        match code.machine_command(&m, format!("gw-leave-{machine}-{suffix}"), by.clone(), remove, None).await {
            Reply::Ok => {}
            Reply::Error(e) if e.code == ErrorCode::NotFound => {}
            other => tracing::info!(machine = %machine, reply = ?other, "a machine leaving the gateway didn't remove its endpoint"),
        }
    }
    // The host revokes its key: that has to happen (a key that still works is a key someone has).
    if let Some(client) = &entry.client
        && let Some(host) = store::machine(db, &p.host.0)?.filter(|h| h.owner_id == o.id && !h.revoked)
    {
        match code.machine_command(&host, format!("gw-revoke-{machine}-{suffix}"), by, Command::GatewayClientRevoke { client: client.clone() }, None).await {
            Reply::Ok => {}
            Reply::Error(e) if e.code == ErrorCode::NotFound => {}
            other => return Err(record_failure(&code, &o.id, &machine, &entry.url, &entry.providers, "revoking its key on the gateway's host", other)),
        }
    }
    let saved = code.db.sql().write(|c| -> Result<GatewayPlan> {
        let Some(mut p) = plan_on(c, &o.id)? else { return Err(no_plan()) };
        p.members.retain(|m| m.machine.0 != machine);
        p.updated_at = now();
        save_on(c, &o.id, &p)?;
        Ok(p)
    })?;
    Ok(Json(saved))
}

/// A sandbox said `hello`: with `new_sandboxes`, it joins the owner's gateway (in the background;
/// the link never waits on it). Machines that aren't sandboxes, the host itself and machines that
/// joined are left alone; a join that failed is tried again at the sandbox's next `hello`.
pub(crate) fn sandbox_hello(code: &Arc<Code>, machine: &MachineRow) {
    if machine.sandbox.is_none() {
        return;
    }
    let p = match plan(code.db.sql(), &machine.owner_id) {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => return tracing::warn!(machine = %machine.id, error = %e, "reading the owner's gateway"),
    };
    if !p.new_sandboxes || p.host.0 == machine.id || p.member(&MachineId(machine.id.clone())).is_some_and(|m| m.client.is_some()) {
        return;
    }
    let (code, id, owner_id) = (code.clone(), machine.id.clone(), machine.owner_id.clone());
    tokio::spawn(async move {
        // The row as this hello left it (its seal key and providers).
        let m = match store::machine(code.db.sql(), &id) {
            Ok(Some(m)) if !m.revoked => m,
            _ => return,
        };
        match join_machine(&code, &owner_id, &m, None, Vec::new(), RELAY).await {
            Ok(_) => tracing::info!(machine = %id, "a new sandbox joined the gateway"),
            Err(e) => tracing::info!(machine = %id, error = %e, "a new sandbox didn't join the gateway"),
        }
    });
}

/// `http(s)://` to `localhost`, `127.x.x.x` or `[::1]`.
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_urls() {
        let v4 = std::net::Ipv4Addr::LOCALHOST;
        assert!(is_loopback_url(&format!("http://{v4}:8790")));
        assert!(is_loopback_url("http://localhost:8790/"));
        assert!(is_loopback_url("http://[::1]:8790"));
        assert!(!is_loopback_url(&format!("http://{}:8790", std::net::Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_loopback_url(&format!("http://u@{v4}:8790")));
        assert!(!is_loopback_url("ftp://localhost"));
    }
}
