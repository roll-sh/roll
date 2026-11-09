//! `/v1/code/agents`: device login, machines, the daemon link, the state snapshot, the event
//! stream and commands (docs/code/http-api.md, "Agents").

use super::ApiRouter;
use super::share;
use super::store::{self, Filter};
use crate::api::{Body, Q, stream_key};
use crate::model::Owner;
use crate::{Caller, Code, Error, Result, hash_token};
use agent_proto::{CommandRequest, ErrorCode, LoginPoll, Mirrored, Reply, State as Snapshot, StreamEvent};
use axum::Extension;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

type S = State<Arc<Code>>;

/// Largest frame a daemon may send (an entities batch, a diff reply).
pub const MAX_FRAME: usize = 16 << 20;
/// Threads one client may ask detail for.
const MAX_THREADS: usize = 200;

pub fn routes() -> Router<Arc<Code>> {
    Router::new()
        .route("/v1/code/agents/login", post(login))
        .route("/v1/code/agents/login/poll", post(poll))
        .route("/v1/code/agents/connect", get(connect).delete(disconnect))
        .route("/v1/code/agents/shares/{id}", get(share::public))
        .route("/v1/code/agents/{owner}/login/approve", post(approve))
        .route("/v1/code/agents/{owner}/login/deny", post(deny))
        .route("/v1/code/agents/{owner}/login/{user_code}", get(pending_login))
        .route("/v1/code/agents/{owner}/machines", get(list_machines).post(add_machine))
        .route("/v1/code/agents/{owner}/machines/{machine}", patch(update_machine).delete(revoke_machine))
        .route("/v1/code/agents/{owner}/machines/{machine}/commands", post(command))
        .route("/v1/code/agents/{owner}/machines/{machine}/projects", post(create_project))
        .route("/v1/code/agents/{owner}/threads/{thread}/work", post(take_work).delete(drop_work))
        .route("/v1/code/agents/{owner}/state", get(state))
        .route("/v1/code/agents/{owner}/events", get(events))
        .route("/v1/code/agents/{owner}/canvas", get(super::docs::get_canvas).put(super::docs::put_canvas))
        .route("/v1/code/agents/{owner}/settings", get(super::docs::get_settings).put(super::docs::put_settings))
        .route("/v1/code/agents/{owner}/endpoints", get(super::endpoints::list_profiles))
        .route("/v1/code/agents/{owner}/endpoints/{id}", put(super::endpoints::put_profile).delete(super::endpoints::delete_profile))
        .route("/v1/code/agents/{owner}/gateway", get(super::gateway::get_plan).put(super::gateway::put_plan).delete(super::gateway::delete_plan))
        .route("/v1/code/agents/{owner}/gateway/members", post(super::gateway::join))
        .route("/v1/code/agents/{owner}/gateway/members/{machine}", delete(super::gateway::leave))
        .route("/v1/code/agents/{owner}/threads/{thread}/shares", post(share::create))
        .route("/v1/code/agents/{owner}/shares", get(share::list))
        .route("/v1/code/agents/{owner}/shares/{id}", delete(share::revoke))
        .merge(super::sandbox::api::routes())
        .merge(super::automations::api::routes())
        .merge(super::linear::api::routes())
        .merge(super::lookup::routes())
}

// ── who may do what ──

/// Reading an owner's agents (machines, state, events): the owner's tokens, agents included.
/// Everyone else gets a 404, so they don't learn what's there.
pub(super) fn readable(code: &Code, caller: &Caller, owner: &str) -> Result<Owner> {
    let o = code.owner_or_404(owner)?;
    if !Code::owns(caller, &o.id) {
        return Err(Error::NotFound(format!("owner {owner}")));
    }
    Ok(o)
}

/// Driving an owner's agents (commands): the owner's tokens, agents included. What an agent's
/// command may do is narrowed by `policy::agent_may`.
fn drivable(code: &Code, caller: &Caller, owner: &str) -> Result<Owner> {
    let o = code.owner_or_404(owner)?;
    match caller {
        _ if Code::owns(caller, &o.id) => Ok(o),
        Caller::Anonymous => Err(Error::Unauthorized),
        _ => Err(Error::Forbidden(format!("not allowed to drive {owner}'s agents"))),
    }
}

/// Adding, renaming and revoking machines: the owner, not its agents.
pub(crate) fn manageable(code: &Code, caller: &Caller, owner: &str) -> Result<Owner> {
    let o = code.owner_or_404(owner)?;
    match caller {
        _ if Code::manages(caller, &o.id) => Ok(o),
        Caller::Anonymous => Err(Error::Unauthorized),
        c if c.agent().is_some() => Err(Error::Forbidden(format!("agent tokens can't manage {owner}'s machines"))),
        _ => Err(Error::Forbidden(format!("not allowed to manage {owner}'s machines"))),
    }
}

/// A machine name: trimmed, one line, at most 64 characters.
pub(crate) fn machine_name(v: Option<&Value>, default: &str) -> Result<String> {
    let name = match v {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) => s.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(64).collect(),
        Some(_) => return Err(Error::Invalid("name must be a string".into())),
    };
    if name.is_empty() {
        return Err(Error::Invalid("name can't be empty".into()));
    }
    Ok(name)
}

/// A `PATCH …/machines/{machine}` body: each key that is there changes that field.
fn machine_patch(b: &Value) -> Result<store::MachinePatch> {
    let o = b.as_object().ok_or_else(|| Error::Invalid("the body is an object".into()))?;
    if !["name", "color", "kind"].iter().any(|k| o.contains_key(*k)) {
        return Err(Error::Invalid("say what to change: name, color or kind".into()));
    }
    fn field<T: serde::de::DeserializeOwned>(o: &serde_json::Map<String, Value>, k: &str) -> Result<Option<Option<T>>> {
        o.get(k)
            .map(|v| serde_json::from_value(v.clone()).map_err(|_| Error::Invalid(format!("{k} {v} isn't one roll knows"))))
            .transpose()
    }
    Ok(store::MachinePatch {
        name: o.get("name").map(|v| machine_name(Some(v).filter(|v| !v.is_null()), "")).transpose()?,
        color: field(o, "color")?,
        kind: field(o, "kind")?,
    })
}

pub(super) fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("system randomness");
    hex::encode(b)
}

/// A machine token: `rlm_` and 48 hex characters.
pub(crate) fn new_machine_token() -> String {
    format!("rlm_{}", random_hex(24))
}

/// `XXXX-XXXX` from letters that can't spell words or be confused with each other.
fn new_user_code() -> String {
    const ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("system randomness");
    let s: String = b.iter().map(|x| ALPHABET[(*x as usize) % ALPHABET.len()] as char).collect();
    format!("{}-{}", &s[..4], &s[4..])
}

/// A user code as typed: any case, with or without the dash.
fn normalize_user_code(s: &str) -> String {
    let s: String = s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    if s.len() == 8 { format!("{}-{}", &s[..4], &s[4..]) } else { s }
}

// ── device login ──

/// An optional one-line field of what a machine says about itself (at most 64 characters).
fn machine_detail(v: Option<&Value>, what: &str) -> Result<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(_)) => Ok(machine_name(v, "").ok()),
        Some(_) => Err(Error::Invalid(format!("{what} must be a string"))),
    }
}

/// Who started a login, for its per-client cap: the client's address as event streams count it
/// (`X-Forwarded-For` only when the operator trusts the proxy; another node's verified client in
/// a cluster), with an IPv6 address's /64 standing for it (one host often has a whole /64).
fn login_client(code: &Code, parts: &Parts) -> String {
    let key = crate::api::stream_key(code, &Caller::Anonymous, parts);
    match key.strip_prefix("ip:").and_then(|ip| ip.parse::<std::net::IpAddr>().ok()) {
        Some(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("ip:{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        Some(std::net::IpAddr::V6(v6)) => format!("ip:{}", v6.to_ipv4_mapped().map_or_else(|| v6.to_string(), |v4| v4.to_string())),
        _ => key,
    }
}

async fn login(State(code): S, parts: Parts, Body(b): Body) -> Result<Response> {
    let name = machine_name(b.get("name"), "machine")?;
    let hostname = machine_detail(b.get("hostname"), "hostname")?;
    let os = machine_detail(b.get("os"), "os")?;
    let m = store::LoginMachine { name: &name, hostname: hostname.as_deref(), os: os.as_deref() };
    let client = login_client(&code, &parts);
    for _ in 0..8 {
        let device_code = format!("rld_{}", random_hex(32));
        let user_code = new_user_code();
        match store::create_login(code.db.sql(), &hash_token(&device_code), &user_code, &m, &client, store::LOGIN_CAPS)? {
            store::NewLogin::Created => return Ok(Json(store::login_response(device_code, user_code, &code.config.web_url)).into_response()),
            store::NewLogin::CodeTaken => continue,
            store::NewLogin::Busy => {
                let msg = format!("{} logins from this address are waiting for approval; finish or let one expire first", store::MAX_PENDING_PER_CLIENT);
                return Ok(*crate::streams::too_many(&msg));
            }
        }
    }
    Err(Error::Unavailable("couldn't pick a free login code; try again".into()))
}

async fn poll(State(code): S, Body(b): Body) -> Result<Json<LoginPoll>> {
    let device_code = b.get("device_code").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| Error::Invalid("device_code is required".into()))?;
    let minted = new_machine_token();
    let machine_id = crate::new_id("mch");
    let (mut poll, added) = store::poll_login(code.db.sql(), &hash_token(device_code), &machine_id, &hash_token(&minted))?;
    if let LoginPoll::Approved { token: t, .. } = &mut poll {
        *t = minted;
    }
    if let Some(m) = added {
        let hub = code.agents.hub(&m.owner_id);
        let _order = hub.order.lock().await;
        code.publish_machine(&m);
    }
    Ok(Json(poll))
}

/// `GET …/login/{user_code}`: which machine a pending login is, before approving it.
async fn pending_login(State(code): S, caller: Caller, Path((owner, user_code)): Path<(String, String)>) -> Result<Json<agent_proto::PendingLogin>> {
    manageable(&code, &caller, &owner)?;
    let user_code = normalize_user_code(&user_code);
    store::pending_login(code.db.sql(), &user_code)?.map(Json).ok_or_else(|| Error::NotFound(format!("login {user_code} (it may have expired)")))
}

async fn approve(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<StatusCode> {
    decide(&code, &caller, &owner, &b, true)
}

async fn deny(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<StatusCode> {
    decide(&code, &caller, &owner, &b, false)
}

fn decide(code: &Code, caller: &Caller, owner: &str, b: &Value, approve: bool) -> Result<StatusCode> {
    let o = manageable(code, caller, owner)?;
    let user_code = b.get("user_code").and_then(Value::as_str).map(normalize_user_code).filter(|s| !s.is_empty()).ok_or_else(|| Error::Invalid("user_code is required".into()))?;
    let name = match b.get("name") {
        None | Some(Value::Null) => None,
        v => Some(machine_name(v, "machine")?),
    };
    if !store::decide_login(code.db.sql(), &user_code, &o.id, approve, name.as_deref())? {
        return Err(Error::NotFound(format!("login {user_code} (it may have expired)")));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── machines ──

async fn list_machines(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Vec<agent_proto::Machine>>> {
    let o = readable(&code, &caller, &owner)?;
    Ok(Json(store::machines(code.db.sql(), &o.id)?.iter().map(|m| code.machine_json(m)).collect()))
}

/// "Add machine" in the dashboard: a machine and its token, at once (sandboxes and scripts).
async fn add_machine(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let o = manageable(&code, &caller, &owner)?;
    let name = machine_name(b.get("name"), "machine")?;
    let minted = new_machine_token();
    let hub = code.agents.hub(&o.id);
    let _order = hub.order.lock().await;
    match store::add_machine(code.db.sql(), &crate::new_id("mch"), &o.id, &name, &hash_token(&minted))? {
        Some(m) => {
            code.publish_machine(&m);
            Ok((StatusCode::CREATED, Json(json!({ "machine": code.machine_json(&m), "token": minted }))))
        }
        None => Err(Error::Unprocessable(format!("{owner} has {} machines; remove one first", store::MAX_MACHINES))),
    }
}

/// Rename a machine or change its colour and kind: `{name?, color?, kind?}`, at least one; a
/// `null` colour or kind clears it (back to the one made from its id and OS).
async fn update_machine(State(code): S, caller: Caller, Path((owner, machine)): Path<(String, String)>, Body(b): Body) -> Result<Json<agent_proto::Machine>> {
    let o = manageable(&code, &caller, &owner)?;
    let patch = machine_patch(&b)?;
    let hub = code.agents.hub(&o.id);
    let _order = hub.order.lock().await;
    let m = store::update_machine(code.db.sql(), &o.id, &machine, &patch)?.ok_or_else(|| Error::NotFound(format!("machine {machine}")))?;
    code.publish_machine(&m);
    Ok(Json(code.machine_json(&m)))
}

/// Revoke a machine: its token stops working and its link closes now.
async fn revoke_machine(State(code): S, caller: Caller, Path((owner, machine)): Path<(String, String)>) -> Result<StatusCode> {
    let o = manageable(&code, &caller, &owner)?;
    if !revoke(&code, &o.id, &machine).await? {
        return Err(Error::NotFound(format!("machine {machine}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Revoke a live machine of an owner; false if there's none.
async fn revoke(code: &Code, owner_id: &str, machine: &str) -> Result<bool> {
    let hub = code.agents.hub(owner_id);
    let _order = hub.order.lock().await;
    let Some(cursor) = store::revoke_machine(code.db.sql(), owner_id, machine)? else { return Ok(false) };
    code.agents.kick(machine);
    let _ = hub.events.send(Arc::new(StreamEvent::MachineRemoved { cursor, machine: agent_proto::MachineId(machine.into()) }));
    // A sandbox's machine takes the sandbox with it.
    code.kick_sandboxes(owner_id);
    Ok(true)
}

// ── the daemon's link ──

/// The live machine whose token (`rlm_…`) the request carries, looked up every time.
fn machine_of(code: &Code, headers: &axum::http::HeaderMap) -> Result<store::MachineRow> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim());
    let machine = match bearer {
        Some(t) if t.starts_with("rlm_") => store::machine_by_token(code.db.sql(), &hash_token(t))?,
        _ => None,
    };
    machine.ok_or(Error::Unauthorized)
}

/// `GET /v1/code/agents/connect`: a daemon's WebSocket. Only a machine token opens it. Without
/// an upgrade it says which machine the token is (`roll agent login --machine-token`).
async fn connect(
    State(code): S,
    headers: axum::http::HeaderMap,
    ws: std::result::Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Result<Response> {
    let machine = machine_of(&code, &headers)?;
    let Ok(ws) = ws else {
        let owner = store::owner_name(code.db.sql(), &machine.owner_id)?.ok_or_else(|| Error::NotFound("the machine's owner".into()))?;
        return Ok(Json(agent_proto::MachineLogin { machine: code.machine_json(&machine), owner }).into_response());
    };
    super::code::follow(&code);
    Ok(ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| super::link::serve(code, machine, socket)).into_response())
}

/// `DELETE /v1/code/agents/connect`: a machine revokes itself (`roll agent logout`).
async fn disconnect(State(code): S, headers: axum::http::HeaderMap) -> Result<StatusCode> {
    let machine = machine_of(&code, &headers)?;
    revoke(&code, &machine.owner_id, &machine.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── state and events ──

#[derive(Deserialize)]
struct ThreadsQuery {
    threads: Option<String>,
    /// Stream what changed after this cursor (the snapshot's), like `Last-Event-ID`.
    after: Option<u64>,
}

fn filter(threads: Option<&str>) -> Result<Filter> {
    let threads: Vec<String> = threads.unwrap_or("").split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect();
    if threads.len() > MAX_THREADS {
        return Err(Error::Invalid(format!("at most {MAX_THREADS} threads")));
    }
    Ok(Filter { threads })
}

async fn state(State(code): S, caller: Caller, Path(owner): Path<String>, Q(q): Q<ThreadsQuery>) -> Result<Json<Snapshot>> {
    let o = readable(&code, &caller, &owner)?;
    let filter = filter(q.threads.as_deref())?;
    // Published changes as they are now, before the snapshot takes their threads.
    super::code::follow(&code);
    super::code::follow_owner(&code, &o.id).await;
    // The cursor first: anything newer that the snapshot also caught comes again on the stream,
    // which is harmless (upserts).
    let cursor = store::owner_cursor(code.db.sql(), &o.id)?;
    let machines = store::machines(code.db.sql(), &o.id)?.iter().map(|m| code.machine_json(m)).collect();
    let entities = store::snapshot(code.db.sql(), &o.id, &filter)?;
    Ok(Json(Snapshot { cursor, machines, entities }))
}

/// Where a stream is.
struct Stream {
    code: Arc<Code>,
    owner_id: String,
    filter: Filter,
    rx: broadcast::Receiver<Arc<StreamEvent>>,
    /// The newest cursor sent (or skipped as already known).
    high: u64,
    /// Read the table after `high` before going on live.
    catch_up: bool,
    backlog: VecDeque<StreamEvent>,
    _permit: crate::streams::Permit,
}

fn cursor_of(ev: &StreamEvent) -> Option<u64> {
    match ev {
        StreamEvent::Entity(m) => Some(m.cursor),
        StreamEvent::Machine { cursor, .. }
        | StreamEvent::MachineRemoved { cursor, .. }
        | StreamEvent::Canvas { cursor, .. }
        | StreamEvent::Settings { cursor, .. }
        | StreamEvent::Automation { cursor, .. }
        | StreamEvent::AutomationRemoved { cursor, .. }
        | StreamEvent::AutomationRun { cursor, .. } => Some(*cursor),
        StreamEvent::Delta { .. } | StreamEvent::Notice { .. } => None,
    }
}

fn kind_of(ev: &StreamEvent) -> &'static str {
    match ev {
        StreamEvent::Entity(_) => "entity",
        StreamEvent::Machine { .. } => "machine",
        StreamEvent::MachineRemoved { .. } => "machine_removed",
        StreamEvent::Delta { .. } => "delta",
        StreamEvent::Canvas { .. } => "canvas",
        StreamEvent::Settings { .. } => "settings",
        StreamEvent::Automation { .. } => "automation",
        StreamEvent::AutomationRemoved { .. } => "automation_removed",
        StreamEvent::AutomationRun { .. } => "automation_run",
        StreamEvent::Notice { .. } => "notice",
    }
}

impl Stream {
    fn wants(&self, ev: &StreamEvent) -> bool {
        match ev {
            StreamEvent::Entity(m) => self.filter.wants(&m.entity),
            StreamEvent::Machine { .. }
            | StreamEvent::MachineRemoved { .. }
            | StreamEvent::Canvas { .. }
            | StreamEvent::Settings { .. }
            | StreamEvent::Automation { .. }
            | StreamEvent::AutomationRemoved { .. }
            | StreamEvent::AutomationRun { .. }
            | StreamEvent::Notice { .. } => true,
            StreamEvent::Delta { delta, .. } => match delta {
                agent_proto::Delta::Text { thread, .. } | agent_proto::Delta::Output { thread, .. } => self.filter.threads.iter().any(|t| t == thread.as_str()),
                agent_proto::Delta::Terminal { .. } => true,
            },
        }
    }

    /// One page of what changed after `high`, from the table, into the backlog.
    fn read_page(&mut self) -> Result<()> {
        let db = self.code.db.sql();
        let (entities, more_entities) = store::entities_after(db, &self.owner_id, self.high, &self.filter)?;
        let machines = store::machines_after(db, &self.owner_id, self.high)?;
        let more_machines = machines.len() as i64 == store::PAGE;
        let mut evs: Vec<StreamEvent> = entities.into_iter().map(|m: Mirrored| StreamEvent::Entity(Box::new(m))).collect();
        for m in &machines {
            evs.push(if m.revoked {
                StreamEvent::MachineRemoved { cursor: m.cursor, machine: agent_proto::MachineId(m.id.clone()) }
            } else {
                StreamEvent::Machine { cursor: m.cursor, machine: self.code.machine_json(m) }
            });
        }
        // One row per owner and document: its last save, if it's newer.
        for kind in super::docs::Doc::ALL {
            if let Some(d) = super::docs::after(db, kind, &self.owner_id, self.high)? {
                evs.push(d.event(kind));
            }
        }
        // Automations and their runs (docs/code/automations.md).
        let automations = super::automations::defs::after(db, &self.owner_id, self.high, store::PAGE)?;
        let more_automations = (automations.len() as i64 == store::PAGE).then(|| automations.last().map_or(0, |a| a.cursor));
        for a in &automations {
            evs.push(match a.deleted_at {
                Some(_) => StreamEvent::AutomationRemoved { cursor: a.cursor, automation: agent_proto::AutomationId(a.id.clone()) },
                None => StreamEvent::Automation { cursor: a.cursor, automation: Box::new(super::automations::defs::view(&self.code, a)) },
            });
        }
        let runs = super::automations::engine::runs_after(db, &self.owner_id, self.high, store::PAGE)?;
        let more_runs = (runs.len() as i64 == store::PAGE).then(|| runs.last().map_or(0, |r| r.cursor));
        evs.extend(runs.into_iter().map(|r| StreamEvent::AutomationRun { cursor: r.cursor, run: Box::new(r) }));
        evs.sort_by_key(|e| cursor_of(e).unwrap_or(0));
        // A full page ends where its last row is; the other tables' rows past it come next time.
        let limit = [more_entities, more_machines.then(|| machines.last().map_or(0, |m| m.cursor)), more_automations, more_runs].into_iter().flatten().min();
        let mut high = self.high;
        for ev in evs {
            let c = cursor_of(&ev).unwrap_or(0);
            if limit.is_some_and(|l| c > l) {
                break;
            }
            high = high.max(c);
            self.backlog.push_back(ev);
        }
        if let Some(l) = limit {
            high = high.max(l);
        }
        self.high = high;
        self.catch_up = limit.is_some();
        Ok(())
    }

    async fn next(&mut self) -> Option<StreamEvent> {
        loop {
            if let Some(ev) = self.backlog.pop_front() {
                return Some(ev);
            }
            if self.catch_up {
                if let Err(e) = self.read_page() {
                    tracing::warn!(error = %e, "agents event stream: catching up");
                    return None;
                }
                continue;
            }
            match self.rx.recv().await {
                Ok(ev) => {
                    if let Some(c) = cursor_of(&ev) {
                        if c <= self.high {
                            continue;
                        }
                        self.high = c;
                    }
                    if self.wants(&ev) {
                        return Some((*ev).clone());
                    }
                }
                // Missed some: the table has every durable one (deltas are only smoothness).
                Err(broadcast::error::RecvError::Lagged(_)) => self.catch_up = true,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// `GET /v1/code/agents/{owner}/events`: SSE. Durable events carry their cursor as `id:`; a
/// reconnect with `Last-Event-ID` (or `?after=`) first gets everything after it from the table.
async fn events(State(code): S, caller: Caller, parts: Parts, Path(owner): Path<String>, Q(q): Q<ThreadsQuery>) -> Result<Response> {
    let o = readable(&code, &caller, &owner)?;
    let filter = filter(q.threads.as_deref())?;
    super::code::follow(&code);
    // Both carry "the newest cursor this client has": resume after the newer of the two (a
    // reconnecting EventSource sends a fresh Last-Event-ID with the URL it first opened). A
    // Last-Event-ID that isn't a cursor is ignored: browsers may send an empty one.
    let last = parts.headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    let after = last.max(q.after);
    let permit = match code.stream_permit(stream_key(&code, &caller, &parts)) {
        Ok(p) => p,
        Err(resp) => return Ok(*resp),
    };
    let rx = code.agents.hub(&o.id).events.subscribe();
    let high = match after {
        Some(a) => a,
        None => store::owner_cursor(code.db.sql(), &o.id)?,
    };
    let s = Stream { code: code.clone(), owner_id: o.id, filter, rx, high, catch_up: after.is_some(), backlog: VecDeque::new(), _permit: permit };
    let stream = futures::stream::unfold(s, |mut s| async move {
        let ev = s.next().await?;
        let mut sse = SseEvent::default().event(kind_of(&ev)).data(serde_json::to_string(&ev).unwrap_or_default());
        if let Some(c) = cursor_of(&ev) {
            sse = sse.id(c.to_string());
        }
        Some((Ok::<_, std::convert::Infallible>(sse), s))
    });
    // Something at once: an engine that resolves `fetch` only on the first body bytes (WebKit)
    // otherwise showed "Reconnecting…" until the first record or keep-alive (up to 15 s).
    let opened = futures::stream::once(async { Ok::<_, std::convert::Infallible>(SseEvent::default().comment("open")) });
    Ok(Sse::new(futures::StreamExt::chain(opened, stream)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

// ── commands ──

/// `POST …/machines/{machine}/commands`: forward a command to the daemon and answer its reply.
/// A daemon's error, the machine being offline or not answering are `Reply::Error`s too, with
/// a status to match and `error` set. Commands that work on a roll code repo get a grant for it
/// (`code.rs`), and a published branch's change is added to the reply.
async fn command(
    State(code): S,
    caller: Caller,
    headers: HeaderMap,
    api: Option<Extension<ApiRouter>>,
    Path((owner, machine)): Path<(String, String)>,
    Body(b): Body,
) -> Result<Response> {
    let o = drivable(&code, &caller, &owner)?;
    let req: CommandRequest = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid command: {e}")))?;
    if req.id.trim().is_empty() || req.id.len() > 128 {
        return Err(Error::Invalid("id must be 1 to 128 characters".into()));
    }
    let m = live_machine(&code, &o, &machine)?;
    if caller.agent().is_some() {
        let current = match &req.command {
            agent_proto::Command::ThreadConfigure { thread, .. } => {
                store::thread_of(code.db.sql(), &o.id, &thread.0)?.filter(|(mid, _)| *mid == m.id).map(|(_, t)| (t.runtime_mode, t.interaction_mode))
            }
            _ => None,
        };
        super::policy::agent_may(&req.command, current).map_err(Error::Forbidden)?;
    }
    // Machine settings never carry a credential: refused here, before they reach the machine
    // (the daemon checks again).
    match &req.command {
        agent_proto::Command::MachineConfigure { settings } => {
            (**settings).clone().check().map_err(Error::Invalid)?;
        }
        // An endpoint's key arrives sealed to the daemon: the relay can only check its shape
        // and forwards it without keeping it (docs/code/agents.md, "Endpoints").
        agent_proto::Command::EndpointSet { endpoint, key } => {
            endpoint.clone().check().map_err(Error::Invalid)?;
            if let agent_proto::KeyChange::Set { sealed } = key {
                sealed.check().map_err(Error::Invalid)?;
            }
        }
        // The gateway's settings hold no secret; an upstream's key arrives sealed to the host, and
        // a client key leaves it sealed to the machine named by `seal_key` (docs/code/gateway.md).
        agent_proto::Command::GatewayConfigure { settings } => {
            (**settings).clone().check().map_err(Error::Invalid)?;
        }
        agent_proto::Command::GatewayKey { key: agent_proto::KeyChange::Set { sealed }, .. } => {
            sealed.check().map_err(Error::Invalid)?;
        }
        agent_proto::Command::GatewayClientIssue { seal_key, .. } => {
            check_seal_key(seal_key)?;
        }
        _ => {}
    }
    super::code::follow(&code);
    let granted = super::code::grant_for(&code, &caller, &m, &req.command).await?;
    let (grant, repo) = granted.map_or((None, None), |(g, r)| (Some(g), Some(r)));
    let reply = code.machine_command(&m, req.id, sender(&caller), req.command.clone(), grant).await;
    let reply = match repo {
        Some(r) => super::code::published(&code, api.as_ref().map(|e| &e.0), &headers, &m, &req.command, &r, reply).await,
        None => reply,
    };
    Ok(reply_response(reply))
}

/// A machine's seal key as `hello` says it: 32 bytes, base64url without padding.
fn check_seal_key(key: &str) -> Result<()> {
    use base64::Engine;
    let ok = key.len() == 43 && base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(key).is_ok_and(|b| b.len() == 32);
    if !ok {
        return Err(Error::Invalid("seal_key is a machine's seal key (32 bytes, base64url)".into()));
    }
    Ok(())
}

/// Who a command is from, for the daemon: the owner's name, `agent:<name>`, or `admin`.
pub(super) fn sender(caller: &Caller) -> String {
    match caller.agent() {
        Some(a) => format!("agent:{a}"),
        None => caller.identity().unwrap_or_else(|| "anonymous".into()),
    }
}

/// One of the owner's live machines, else a 404.
pub(super) fn live_machine(code: &Code, o: &Owner, machine: &str) -> Result<store::MachineRow> {
    match store::machine(code.db.sql(), machine)? {
        Some(m) if m.owner_id == o.id && !m.revoked => Ok(m),
        _ => Err(Error::NotFound(format!("machine {machine}"))),
    }
}

/// `POST …/machines/{machine}/projects {id, repo: {name, description?, visibility?}, path?, name?}`:
/// a new roll code repo, cloned onto the machine as a project.
async fn create_project(
    State(code): S,
    caller: Caller,
    headers: HeaderMap,
    api: Option<Extension<ApiRouter>>,
    Path((owner, machine)): Path<(String, String)>,
    Body(b): Body,
) -> Result<Response> {
    let o = drivable(&code, &caller, &owner)?;
    if caller.agent().is_some() {
        let add = agent_proto::Command::ProjectAdd { path: String::new(), name: None, clone: None };
        super::policy::agent_may(&add, None).map_err(Error::Forbidden)?;
    }
    let m = live_machine(&code, &o, &machine)?;
    let reply = super::code::create_project(&code, api.as_ref().map(|e| &e.0), &caller, &headers, &o, &m, &b).await?;
    Ok(reply_response(reply))
}

/// `POST …/threads/{thread}/work {work? | repo, labels?, lease_s?, agent?}` → WorkItem | 204.
async fn take_work(
    State(code): S,
    caller: Caller,
    headers: HeaderMap,
    api: Option<Extension<ApiRouter>>,
    Path((owner, thread)): Path<(String, String)>,
    Body(b): Body,
) -> Result<Response> {
    let o = drivable(&code, &caller, &owner)?;
    Ok(match super::code::take_work(&code, api.as_ref().map(|e| &e.0), &caller, &headers, &o, &thread, &b).await? {
        Some(w) => Json(w).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

async fn drop_work(State(code): S, caller: Caller, Path((owner, thread)): Path<(String, String)>) -> Result<StatusCode> {
    let o = drivable(&code, &caller, &owner)?;
    super::code::drop_work(&code, &o, &thread).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn reply_status(reply: &Reply) -> StatusCode {
    match reply {
        Reply::Error(e) => match e.code {
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::Provider => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::MachineOffline => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ErrorCode::Internal => StatusCode::BAD_GATEWAY,
        },
        _ => StatusCode::OK,
    }
}

fn reply_response(reply: Reply) -> Response {
    let status = reply_status(&reply);
    let mut body = serde_json::to_value(&reply).unwrap_or(Value::Null);
    if let Reply::Error(e) = &reply {
        body["error"] = json!(e.message);
    }
    (status, Json(body)).into_response()
}
