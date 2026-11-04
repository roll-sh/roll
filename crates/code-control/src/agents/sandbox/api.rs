//! `/v1/code/agents/{owner}/sandboxes` (docs/code/http-api.md, "Sandboxes"): list, create, start,
//! stop and delete an owner's sandboxes, and their running time for billing. Every route needs
//! `Code::manages` (the owner or admin, not its agents): sandboxes cost money.

use super::reconcile::seal_scope;
use super::store::{self, Created, NewSandbox, SandboxRow};
use super::{Desired, price_usd, round_usd};
use crate::agents::api::{machine_name, manageable, new_machine_token};
use crate::api::{Body, Q};
use crate::{Caller, Code, Error, Result, hash_token};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

type S = State<Arc<Code>>;

/// Variables an owner may give a sandbox, and how big they may be.
const MAX_ENV: usize = 32;
const MAX_ENV_VALUE: usize = 4096;
const MAX_ENV_BYTES: usize = 32 << 10;

pub fn routes() -> Router<Arc<Code>> {
    Router::new()
        .route("/v1/code/agents/{owner}/sandboxes", get(list).post(create))
        .route("/v1/code/agents/{owner}/sandboxes/usage", get(usage))
        .route("/v1/code/agents/{owner}/sandboxes/{id}", delete(remove))
        .route("/v1/code/agents/{owner}/sandboxes/{id}/start", post(start))
        .route("/v1/code/agents/{owner}/sandboxes/{id}/stop", post(stop))
}

/// A sandbox as the API shows it. Its env never leaves the server.
pub fn sandbox_json(s: &SandboxRow) -> Value {
    json!({
        "id": s.id,
        "name": s.name,
        "machine": s.machine_id,
        "backend": s.backend,
        "size": s.size,
        "status": store::status_str(s.status),
        "desired": s.desired,
        "error": s.error,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

/// The owner's env for a sandbox: `{NAME: "value"}`, names like `ANTHROPIC_API_KEY`. roll's own
/// variables (`ROLL_*`) and the backend's (`AGENT37_*`) are refused, as are ones that would change
/// who or where the sandbox's user is.
fn parse_env(v: Option<&Value>) -> Result<BTreeMap<String, String>> {
    let obj = match v {
        None | Some(Value::Null) => return Ok(BTreeMap::new()),
        Some(Value::Object(o)) => o,
        Some(_) => return Err(Error::Invalid("env is an object of NAME: \"value\"".into())),
    };
    if obj.len() > MAX_ENV {
        return Err(Error::Invalid(format!("env has at most {MAX_ENV} variables")));
    }
    let mut out = BTreeMap::new();
    let mut total = 0;
    for (k, v) in obj {
        let well_formed = k.len() <= 128 && k.starts_with(|c: char| c.is_ascii_uppercase()) && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !well_formed {
            return Err(Error::Invalid(format!("env name {k:?}: uppercase letters, digits and _, starting with a letter")));
        }
        if ["ROLL_", "AGENT37_", "OPENCLAW_"].iter().any(|p| k.starts_with(p)) || ["HOME", "USER", "PATH", "SHELL", "IS_SANDBOX"].contains(&k.as_str()) {
            return Err(Error::Invalid(format!("env name {k} is reserved")));
        }
        let Some(v) = v.as_str() else { return Err(Error::Invalid(format!("env {k} must be a string"))) };
        if v.chars().count() > MAX_ENV_VALUE || v.contains(['\n', '\r', '\0']) {
            return Err(Error::Invalid(format!("env {k}: one line of at most {MAX_ENV_VALUE} characters")));
        }
        total += k.len() + v.len();
        out.insert(k.clone(), v.to_string());
    }
    if total > MAX_ENV_BYTES {
        return Err(Error::Invalid(format!("env is at most {} KiB", MAX_ENV_BYTES >> 10)));
    }
    Ok(out)
}

/// `GET …/sandboxes`: the owner's sandboxes, the sizes on offer and the limit.
async fn list(State(code): S, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    let rows = store::live(code.db.sql(), &o.id)?;
    let cfg = &code.config.sandboxes;
    Ok(Json(json!({
        "sandboxes": rows.iter().map(sandbox_json).collect::<Vec<_>>(),
        "sizes": cfg.sizes,
        "max": cfg.max_per_owner,
        "backend": code.sandboxes.backend_name(),
    })))
}

/// `POST …/sandboxes {name?, size?, env?}`: a machine and its sandbox, created in the background
/// (`status: creating` until the backend runs it).
async fn create(State(code): S, caller: Caller, Path(owner): Path<String>, Body(b): Body) -> Result<(StatusCode, Json<Value>)> {
    let o = manageable(&code, &caller, &owner)?;
    let name = machine_name(b.get("name"), "sandbox")?;
    let cfg = &code.config.sandboxes;
    let size = match b.get("size") {
        None | Some(Value::Null) => cfg.sizes.first(),
        Some(Value::String(s)) => cfg.sizes.iter().find(|x| &x.name == s),
        Some(_) => return Err(Error::Invalid("size is a string".into())),
    }
    .ok_or_else(|| Error::Invalid(format!("size is one of {}", cfg.sizes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "))))?
    .clone();
    let env = parse_env(b.get("env"))?;
    let backend = code.sandboxes.backend.clone().ok_or_else(|| Error::Unavailable("sandboxes aren't set up on this server".into()))?;
    let sealer = code.ai.sealer.as_ref().ok_or_else(|| Error::Unprocessable("sandboxes need ROLL_SECRET_KEY on the server".into()))?;
    let (id, machine_id, token) = (crate::new_id("sbx"), crate::new_id("mch"), new_machine_token());
    let api_url = cfg.api_url.clone().unwrap_or_else(|| code.config.api_url.clone());
    let mut full = env;
    full.insert("ROLL_API_URL".into(), api_url);
    full.insert("ROLL_AGENT_TOKEN".into(), token.clone());
    full.insert("ROLL_AGENT_MACHINE".into(), machine_id.clone());
    full.insert("ROLL_AGENT_OWNER".into(), o.name.clone());
    full.insert("ROLL_AGENT_NAME".into(), name.clone());
    let sealed = sealer.seal(&seal_scope(&id), &serde_json::to_string(&full).unwrap_or_default());
    drop(full);
    let hub = code.agents.hub(&o.id);
    let order = hub.order.lock().await;
    let new = NewSandbox {
        id: &id,
        owner_id: &o.id,
        machine_id: &machine_id,
        name: &name,
        backend: backend.name(),
        size: &size,
        token_hash: &hash_token(&token),
        sealed: &sealed,
        max: cfg.max_per_owner,
    };
    let (s, m) = match store::create(code.db.sql(), &new)? {
        Created::Ok(s, m) => (s, m),
        Created::TooMany => return Err(Error::Unprocessable(format!("{owner} has {} sandboxes; delete one first", cfg.max_per_owner))),
        Created::TooManyMachines => return Err(Error::Unprocessable(format!("{owner} has {} machines; remove one first", crate::agents::store::MAX_MACHINES))),
    };
    code.publish_machine(&m);
    drop(order);
    code.kick_sandboxes(&o.id);
    Ok((StatusCode::CREATED, Json(json!({ "sandbox": sandbox_json(&s), "machine": code.machine_json(&m) }))))
}

/// The owner's live sandbox `id`.
fn find(code: &Code, owner_id: &str, id: &str) -> Result<SandboxRow> {
    store::sandbox(code.db.sql(), id)?.filter(|s| s.owner_id == owner_id && s.deleted_at.is_none()).ok_or_else(|| Error::NotFound(format!("sandbox {id}")))
}

/// Ask for `desired`, then look at it at once.
async fn want(code: &Code, owner_id: &str, id: &str, desired: Desired) -> Result<Option<SandboxRow>> {
    if code.sandboxes.backend.is_none() {
        return Err(Error::Unavailable("sandboxes aren't set up on this server".into()));
    }
    let hub = code.agents.hub(owner_id);
    {
        let _order = hub.order.lock().await;
        match store::set_desired(code.db.sql(), owner_id, id, desired)? {
            Some((_, Some(m))) => code.publish_machine(&m),
            Some(_) => {}
            None => return Err(Error::NotFound(format!("sandbox {id}"))),
        }
    }
    code.reconcile_now(owner_id, id).await
}

async fn start(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    lifecycle(&code, &o.id, &id, Desired::Running).await
}

async fn stop(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    lifecycle(&code, &o.id, &id, Desired::Stopped).await
}

async fn lifecycle(code: &Code, owner_id: &str, id: &str, desired: Desired) -> Result<Json<Value>> {
    let s = find(code, owner_id, id)?;
    if s.desired == Desired::Deleted {
        return Err(Error::Conflict(format!("sandbox {id} is being deleted")));
    }
    let s = want(code, owner_id, id, desired).await?.ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
    Ok(Json(sandbox_json(&s)))
}

/// `DELETE …/sandboxes/{id}`: the backend deletes it and its data, and its machine goes. 204 once
/// done; 202 with the sandbox (`deleting`) while the backend is still at it.
async fn remove(State(code): S, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<Response> {
    let o = manageable(&code, &caller, &owner)?;
    find(&code, &o.id, &id)?;
    match want(&code, &o.id, &id, Desired::Deleted).await? {
        Some(s) if s.deleted_at.is_none() => Ok((StatusCode::ACCEPTED, Json(sandbox_json(&s))).into_response()),
        _ => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[derive(Deserialize)]
struct MonthQuery {
    month: Option<String>,
}

/// `GET …/sandboxes/usage?month=YYYY-MM`: running time and what it costs, per sandbox.
async fn usage(State(code): S, caller: Caller, Path(owner): Path<String>, Q(q): Q<MonthQuery>) -> Result<Json<Value>> {
    let o = manageable(&code, &caller, &owner)?;
    let month = match q.month.as_deref() {
        Some(m) if m.len() == 7 && m.as_bytes()[4] == b'-' && m.chars().enumerate().all(|(i, c)| i == 4 || c.is_ascii_digit()) => m.to_string(),
        Some(m) => return Err(Error::Invalid(format!("month is YYYY-MM, not {m}"))),
        None => crate::ai::month_of(crate::db::now()),
    };
    let rows = store::usage(code.db.sql(), &o.id, &month)?;
    let hours = |ms: i64| (ms as f64 / 36_000.0).round() / 100.0;
    let (mut total_ms, mut total_usd) = (0i64, 0.0f64);
    let sandboxes: Vec<Value> = rows
        .iter()
        .map(|u| {
            let usd = price_usd(u.running_ms, u.hourly_cents);
            total_ms += u.running_ms;
            total_usd += usd;
            json!({ "sandbox": u.sandbox_id, "name": u.name, "size": u.size, "hourly_cents": u.hourly_cents, "running_ms": u.running_ms, "running_hours": hours(u.running_ms), "price_usd": round_usd(usd) })
        })
        .collect();
    Ok(Json(json!({
        "owner": o.name,
        "month": month,
        "sandboxes": sandboxes,
        "totals": { "running_ms": total_ms, "running_hours": hours(total_ms), "price_usd": round_usd(total_usd) },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_is_checked() {
        let ok = parse_env(Some(&json!({ "ANTHROPIC_API_KEY": "sk-ant-x", "OPENAI_BASE_URL": "https://x" }))).unwrap();
        assert_eq!(ok.len(), 2);
        assert!(parse_env(None).unwrap().is_empty());
        for bad in [
            json!("x"),
            json!({ "lower": "x" }),
            json!({ "1X": "x" }),
            json!({ "ROLL_AGENT_TOKEN": "x" }),
            json!({ "AGENT37_INSTANCE_ID": "x" }),
            json!({ "PATH": "/x" }),
            json!({ "A": 1 }),
            json!({ "A": "two\nlines" }),
            json!({ "A": "x".repeat(MAX_ENV_VALUE + 1) }),
        ] {
            assert!(parse_env(Some(&bad)).is_err(), "{bad}");
        }
        let many: serde_json::Map<String, Value> = (0..=MAX_ENV).map(|i| (format!("K{i}"), json!("v"))).collect();
        assert!(parse_env(Some(&Value::Object(many))).is_err());
    }
}
