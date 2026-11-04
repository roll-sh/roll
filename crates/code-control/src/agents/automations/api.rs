//! `/v1/code/agents/{owner}/automations…` (docs/code/http-api.md, "Automations"). Reading is the
//! owner's tokens' (agents included; 404 to anyone else); changing automations and their
//! secrets, cancelling and retrying runs is `Code::manages`; running one by hand is too, or an
//! agent token when the automation allows agents. The webhook ingress takes no token: its
//! signature is the credential.

use super::defs::{self, Saved};
use super::engine;
use super::registry::{NewEvent, OwnerRef};
use super::{filter, template, triggers};
use crate::agents::api::{manageable, readable, sender};
use crate::api::{Body, Q};
use crate::model::Owner;
use crate::{Caller, Code, Error, Result};
use agent_proto::automation::{
    AutomationEvent, AutomationEventId, AutomationId, AutomationSaved, AutomationSpec, RunDetail, RunNow, RunState, TestRequest, TestResult, TestStep, Trigger, limits,
};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Code>>;

pub fn routes() -> Router<Arc<Code>> {
    Router::new()
        .route("/v1/code/agents/{owner}/automations", get(list).post(create))
        .route("/v1/code/agents/{owner}/automations/catalog", get(catalog))
        .route("/v1/code/agents/{owner}/automations/test", post(test))
        .route("/v1/code/agents/{owner}/automations/runs", get(owner_runs))
        .route("/v1/code/agents/{owner}/automations/runs/{run}", get(run_detail))
        .route("/v1/code/agents/{owner}/automations/runs/{run}/cancel", post(cancel))
        .route("/v1/code/agents/{owner}/automations/runs/{run}/retry", post(retry))
        .route("/v1/code/agents/{owner}/automations/{id}", get(one).put(update).delete(remove))
        .route("/v1/code/agents/{owner}/automations/{id}/enable", post(enable))
        .route("/v1/code/agents/{owner}/automations/{id}/disable", post(disable))
        .route("/v1/code/agents/{owner}/automations/{id}/run", post(run_now))
        .route("/v1/code/agents/{owner}/automations/{id}/runs", get(automation_runs))
        .route("/v1/code/agents/{owner}/automations/{id}/secrets/{name}", put(set_secret).delete(drop_secret))
        .route("/v1/code/agents/{owner}/automations/{id}/webhook/secret", post(rotate))
        .route("/v1/code/agents/{owner}/automations/{id}/webhook", post(ingress).layer(axum::extract::DefaultBodyLimit::max(limits::EVENT)))
}

fn owner_ref(o: &Owner) -> OwnerRef {
    OwnerRef { id: o.id.clone(), name: o.name.clone() }
}

fn found(code: &Code, o: &Owner, id: &str) -> Result<defs::AutomationRow> {
    match defs::get(code.db.sql(), id)? {
        Some(a) if a.owner_id == o.id && a.deleted_at.is_none() => Ok(a),
        _ => Err(Error::NotFound(format!("automation {id}"))),
    }
}

fn spec_of(mut b: Value) -> Result<(AutomationSpec, Option<u64>)> {
    let revision = b.as_object_mut().and_then(|o| o.remove("revision")).and_then(|v| v.as_u64());
    let spec = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid automation: {e}")))?;
    Ok((spec, revision))
}

fn saved(code: &Code, s: Saved, status: StatusCode) -> Result<Response> {
    Ok(match s {
        Saved::Done { row, webhook_secret } => (status, Json(AutomationSaved { automation: defs::view(code, &row), webhook_secret })).into_response(),
        Saved::Conflict(cur) => {
            (StatusCode::CONFLICT, Json(json!({ "error": format!("the automation changed (it is at revision {})", cur.revision), "current": defs::view(code, &cur) }))).into_response()
        }
        Saved::NotFound => return Err(Error::NotFound("automation".into())),
        Saved::TooMany => return Err(Error::Unprocessable(format!("an owner has at most {} automations", limits::AUTOMATIONS))),
    })
}

async fn list(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    code.start_automations();
    let v: Vec<_> = defs::list(code.db.sql(), &o.id)?.iter().map(|a| defs::view(&code, a)).collect();
    Ok(Json(json!(v)))
}

async fn create(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Response> {
    let o = manageable(&code, &caller, &owner)?;
    code.start_automations();
    let (spec, _) = spec_of(b)?;
    let checked = defs::check(&code, &owner_ref(&o), &spec)?;
    let s = defs::create(&code, &owner_ref(&o), &sender(&caller), checked).await?;
    code.wake_automations(&o.id);
    saved(&code, s, StatusCode::CREATED)
}

async fn one(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    Ok(Json(json!(defs::view(&code, &found(&code, &o, &id)?))))
}

async fn update(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>, Body(b): Body) -> Result<Response> {
    let o = manageable(&code, &caller, &owner)?;
    code.start_automations();
    found(&code, &o, &id)?;
    let (spec, revision) = spec_of(b)?;
    let checked = defs::check(&code, &owner_ref(&o), &spec)?;
    let s = defs::update(&code, &owner_ref(&o), &id, checked, revision).await?;
    code.wake_automations(&o.id);
    saved(&code, s, StatusCode::OK)
}

async fn remove(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<StatusCode> {
    let o = manageable(&code, &caller, &owner)?;
    if !defs::delete(&code, &owner_ref(&o), &id).await? {
        return Err(Error::NotFound(format!("automation {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn toggle(code: Arc<Code>, caller: Caller, owner: String, id: String, on: bool) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    code.start_automations();
    let a = defs::set_enabled(&code, &owner_ref(&o), &id, on).await?.ok_or_else(|| Error::NotFound(format!("automation {id}")))?;
    code.wake_automations(&o.id);
    Ok(Json(json!(defs::view(&code, &a))))
}

async fn enable(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Json<Value>> {
    toggle(code, caller, owner, id, true).await
}

async fn disable(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Json<Value>> {
    toggle(code, caller, owner, id, false).await
}

async fn rotate(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Response> {
    let o = manageable(&code, &caller, &owner)?;
    if !matches!(found(&code, &o, &id)?.spec.trigger, Trigger::Webhook) {
        return Err(Error::NotFound(format!("automation {id} has no webhook")));
    }
    saved(&code, defs::rotate_hook_secret(&code, &owner_ref(&o), &id).await?, StatusCode::OK)
}

fn secret_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
        return Err(Error::Invalid("a secret's name: [A-Z0-9_], 1 to 64".into()));
    }
    Ok(())
}

async fn set_secret(State(code): S, caller: Caller, Path((owner, id, name)): Path<(String, String, String)>, Body(b): Body) -> Result<StatusCode> {
    let o = manageable(&code, &caller, &owner)?;
    secret_name(&name)?;
    let value = b["value"].as_str().ok_or_else(|| Error::Invalid("value must be a string".into()))?;
    if value.is_empty() || value.len() > limits::SECRET {
        return Err(Error::Invalid(format!("value: 1 to {} bytes", limits::SECRET)));
    }
    if !defs::set_secret(&code, &owner_ref(&o), &id, &name, Some(value)).await? {
        return Err(Error::NotFound(format!("automation {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn drop_secret(State(code): S, caller: Caller, Path((owner, id, name)): Path<(String, String, String)>) -> Result<StatusCode> {
    let o = manageable(&code, &caller, &owner)?;
    secret_name(&name)?;
    if !defs::set_secret(&code, &owner_ref(&o), &id, &name, None).await? {
        return Err(Error::NotFound(format!("automation {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn catalog(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    code.start_automations();
    Ok(Json(json!(code.automations.registry.catalog(&owner_ref(&o)))))
}

/// Whether a trigger takes an event (source and kinds; an integration's own say).
fn trigger_takes(code: &Code, t: &Trigger, e: &AutomationEvent) -> bool {
    if t.source() != e.source {
        return false;
    }
    match t {
        Trigger::Code { events, .. } => events.is_empty() || events.contains(&e.kind),
        Trigger::Agent { events } => events.iter().any(|x| x.kind() == e.kind),
        Trigger::Integration { source, events, config } => {
            (events.is_empty() || events.contains(&e.kind)) && code.automations.registry.trigger(source).is_some_and(|t| t.matches(config, e))
        }
        _ => true,
    }
}

async fn test(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<Json<TestResult>> {
    let o = readable(&code, &caller, &owner)?;
    code.start_automations();
    let req: TestRequest = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid test: {e}")))?;
    let e = AutomationEvent {
        id: AutomationEventId("aev_test".into()),
        source: req.event.source.clone(),
        kind: req.event.kind.clone(),
        key: "test".into(),
        at: code_sql::now_ms().max(0) as u64,
        data: req.event.data.clone(),
        automation: None,
    };
    let a = &req.automation;
    let mut ctx = engine::event_ctx(&e, "a test event", &AutomationId("aut_test".into()), &a.name, &o.name);
    ctx["automation"]["defaults"] = json!(a.defaults);
    let trigger = trigger_takes(&code, &a.trigger, &e);
    let filters = filter::verdicts(&a.filters, &ctx);
    let reg = &code.automations.registry;
    let steps = a
        .steps
        .iter()
        .map(|s| {
            let runs = filter::all(&s.when, &ctx);
            let checked = match reg.step(&s.kind) {
                None => Err(format!("no step kind {:?}", s.kind)),
                Some(k) => k.check(&owner_ref(&o), &s.config),
            };
            let rendered = checked.and_then(|_| template::render_json(&s.config, &ctx, template::Secrets::Refused).map_err(|e| e.0));
            match rendered {
                Ok(config) => TestStep { id: s.id.clone(), runs, config, error: None },
                Err(error) => TestStep { id: s.id.clone(), runs, config: Value::Null, error: Some(error) },
            }
        })
        .collect();
    Ok(Json(TestResult { matched: trigger && filters.iter().all(|f| *f), trigger, filters, steps }))
}

#[derive(Deserialize)]
struct RunsQuery {
    automation: Option<String>,
    status: Option<RunState>,
    before: Option<String>,
    limit: Option<i64>,
}

fn limit(l: Option<i64>) -> Result<i64> {
    match l {
        None => Ok(50),
        Some(n) if (1..=200).contains(&n) => Ok(n),
        Some(_) => Err(Error::Invalid("limit: 1 to 200".into())),
    }
}

async fn owner_runs(State(code): S, caller: Caller, Path(owner): Path<String>, Q(q): Q<RunsQuery>) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    let runs = engine::runs(code.db.sql(), &o.id, q.automation.as_deref(), q.status, q.before.as_deref(), limit(q.limit)?)?;
    Ok(Json(json!(runs)))
}

async fn automation_runs(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>, Q(q): Q<RunsQuery>) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    found(&code, &o, &id)?;
    let runs = engine::runs(code.db.sql(), &o.id, Some(&id), q.status, q.before.as_deref(), limit(q.limit)?)?;
    Ok(Json(json!(runs)))
}

async fn run_detail(State(code): S, caller: Caller, Path((owner, run)): Path<(String, String)>) -> Result<Json<RunDetail>> {
    let o = readable(&code, &caller, &owner)?;
    let r = engine::run_of(code.db.sql(), &run)?.filter(|r| r.owner_id == o.id).ok_or_else(|| Error::NotFound(format!("run {run}")))?;
    let event = engine::event_of(code.db.sql(), &r.event_id)?.ok_or_else(|| Error::NotFound(format!("run {run}")))?;
    Ok(Json(RunDetail { run: r.run, event }))
}

async fn cancel(State(code): S, caller: Caller, Path((owner, run)): Path<(String, String)>) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    code.start_automations();
    let r = code.cancel_automation_run(&o.id, &run).await?.ok_or_else(|| Error::NotFound(format!("run {run}")))?;
    Ok(Json(json!(r)))
}

async fn retry(State(code): S, caller: Caller, Path((owner, run)): Path<(String, String)>) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    code.start_automations();
    let r = code.retry_automation_run(&o.id, &run).await?.ok_or_else(|| Error::NotFound(format!("run {run}")))?;
    Ok(Json(json!(r)))
}

/// `POST …/{id}/run`: the owner, or an agent token when the automation allows agents.
async fn run_now(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>, Body(b): Body) -> Result<Response> {
    let o = code.owner_or_404(&owner)?;
    match &caller {
        _ if Code::manages(&caller, &o.id) => {}
        Caller::Anonymous => return Err(Error::Unauthorized),
        c if c.agent().is_some() && Code::owns(c, &o.id) => {
            if !found(&code, &o, &id)?.spec.allow_agents {
                return Err(Error::Forbidden("this automation doesn't let agents run it (allow_agents)".into()));
            }
        }
        _ => return Err(Error::Forbidden(format!("not allowed to run {owner}'s automations"))),
    }
    found(&code, &o, &id)?;
    let req: RunNow = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid run: {e}")))?;
    if req.key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 128) {
        return Err(Error::Invalid("key: 1 to 128 characters".into()));
    }
    code.start_automations();
    let by = sender(&caller);
    let key = req.key.clone().unwrap_or_else(|| crate::new_id("run"));
    let e = NewEvent {
        source: "manual".into(),
        kind: "run".into(),
        id: format!("{id}:{key}"),
        data: json!({ "by": by, "input": req.input }),
        summary: format!("Run by {by}"),
        automation: Some(AutomationId(id.clone())),
    };
    let fresh = code.automation_event(&o.id, e)?.is_some();
    // Match it now (this node serves the owner), so the answer is the run.
    code.match_events(&o.id).await?;
    let ev: Option<String> = code
        .db
        .sql()
        .query_row("SELECT id FROM automation_events WHERE owner_id = ? AND key = ?", [o.id.as_str(), &format!("manual:{id}:{key}")], |r| r.get(0))
        .ok();
    let run_id: Option<String> =
        ev.and_then(|ev| code.db.sql().query_row("SELECT id FROM automation_runs WHERE automation_id = ? AND event_id = ?", [id.as_str(), &ev], |r| r.get(0)).ok());
    let run = run_id.and_then(|r| engine::run_of(code.db.sql(), &r).ok().flatten()).ok_or_else(|| Error::Conflict("the run wasn't made yet (try again with the same key)".into()))?;
    let status = if fresh { StatusCode::ACCEPTED } else { StatusCode::OK };
    Ok((status, Json(json!(run.run))).into_response())
}

/// `POST …/{id}/webhook`: an inbound event, signed with the automation's secret.
async fn ingress(State(code): S, Path((owner, id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> Result<Response> {
    let o = code.owner_or_404(&owner)?;
    let a = found(&code, &o, &id)?;
    if !matches!(a.spec.trigger, Trigger::Webhook) {
        return Err(Error::NotFound(format!("automation {id} has no webhook")));
    }
    let Some(secret) = defs::hook_secret(&code, &a) else {
        return Err(Error::Unprocessable("the webhook's secret can't be opened (ROLL_SECRET_KEY changed?)".into()));
    };
    let key = match triggers::verify(&secret, &headers, &body, code_sql::now_ms() / 1000) {
        Ok(k) => k,
        Err(why) => return Ok((StatusCode::UNAUTHORIZED, Json(json!({ "error": why }))).into_response()),
    };
    if !a.enabled {
        return Err(Error::Conflict("the automation is disabled".into()));
    }
    code.start_automations();
    let data_body = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    let skip = ["authorization", "cookie", "x-roll-signature-256", "x-hub-signature-256", "x-hub-signature", "proxy-authorization"];
    let hdrs: serde_json::Map<String, Value> = headers
        .iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .take(64)
        .map(|(k, v)| (k.as_str().to_string(), json!(String::from_utf8_lossy(v.as_bytes()).chars().take(1024).collect::<String>())))
        .collect();
    let e = NewEvent {
        source: "webhook".into(),
        kind: "post".into(),
        id: format!("{id}:{key}"),
        data: json!({ "body": data_body, "headers": hdrs }),
        summary: "Webhook".into(),
        automation: Some(AutomationId(id.clone())),
    };
    let made = code.automation_event(&o.id, e)?;
    let event: Option<String> = match &made {
        Some(e) => Some(e.0.clone()),
        None => code.db.sql().query_row("SELECT id FROM automation_events WHERE owner_id = ? AND key = ?", [o.id.as_str(), &format!("webhook:{id}:{key}")], |r| r.get(0)).ok(),
    };
    Ok((StatusCode::ACCEPTED, Json(json!({ "event": event, "duplicate": made.is_none() }))).into_response())
}
