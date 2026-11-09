//! The owner's endpoint profiles (docs/code/agents.md, "Endpoints"): endpoints kept on the relay
//! to apply to many machines and to offer for new sandboxes. A profile never holds a key, and
//! roll keeps none: applying one, the browser seals the key to each machine's daemon and sends
//! `endpoint.set` through `…/commands`, which only forwards the ciphertext. A key sent with a
//! profile is dropped by the parser (`EndpointProfile` has no field for it), never stored.
//!
//! - `GET /v1/code/agents/{owner}/endpoints`: the profiles (the owner's tokens, agents included).
//! - `PUT /v1/code/agents/{owner}/endpoints/{id}`: add or replace one (the owner, not its agents).
//! - `DELETE /v1/code/agents/{owner}/endpoints/{id}`: remove one; the machines keep theirs.

use super::api::readable;
use crate::api::Body;
use crate::{Caller, Code, Error, Result};
use agent_proto::{EndpointProfile, endpoint_limits};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use code_sql::{OptionalExtension, params};
use std::sync::Arc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_endpoint_profiles (
    owner_id TEXT NOT NULL,
    id TEXT NOT NULL,
    -- The profile, JSON (`agent_proto::EndpointProfile`): a name, kind, base URL and providers;
    -- never a key.
    json TEXT NOT NULL,
    -- Unix ms.
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (owner_id, id)
);
"#;

fn parse(json: &str) -> Option<EndpointProfile> {
    serde_json::from_str(json).ok()
}

/// The owner's profiles, oldest first; one a newer roll wrote and this one can't read is left out.
pub fn list(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Vec<EndpointProfile>> {
    let rows: Vec<String> = db.query("SELECT json FROM agent_endpoint_profiles WHERE owner_id = ? ORDER BY updated_at, id", [owner_id], |r| r.get(0))?;
    Ok(rows.iter().filter_map(|j| parse(j)).collect())
}

/// Changing profiles: the owner, not its agents (an agent's token works the threads; where the
/// owner's providers send their requests is the owner's).
fn changeable(code: &Code, caller: &Caller, owner: &str) -> Result<crate::model::Owner> {
    let o = code.owner_or_404(owner)?;
    match caller {
        _ if Code::manages(caller, &o.id) => Ok(o),
        Caller::Anonymous => Err(Error::Unauthorized),
        c if c.agent().is_some() => Err(Error::Forbidden(format!("agent tokens can't change {owner}'s endpoints"))),
        _ => Err(Error::Forbidden(format!("not allowed to change {owner}'s endpoints"))),
    }
}

pub(super) async fn list_profiles(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>) -> Result<Json<Vec<EndpointProfile>>> {
    let o = readable(&code, &caller, &owner)?;
    Ok(Json(list(code.db.sql(), &o.id)?))
}

/// `PUT …/endpoints/{id} EndpointProfile`: the path names it. `sandbox_default` on one takes it
/// off the others. At most [`endpoint_limits::ENDPOINTS`] per owner.
pub(super) async fn put_profile(
    State(code): State<Arc<Code>>,
    caller: Caller,
    Path((owner, id)): Path<(String, String)>,
    Body(mut b): Body,
) -> Result<Json<EndpointProfile>> {
    let o = changeable(&code, &caller, &owner)?;
    if let Some(fields) = b.as_object_mut() {
        fields.insert("id".into(), serde_json::Value::String(id));
    }
    let mut profile: EndpointProfile = serde_json::from_value(b).map_err(|e| Error::Invalid(format!("invalid endpoint: {e}")))?;
    profile.spec = profile.spec.check().map_err(Error::Invalid)?;
    profile.updated_at = code_sql::now_ms().max(0) as u64;
    let saved = profile.clone();
    code.db.sql().write(|c| -> Result<()> {
        let exists = c
            .query_row("SELECT 1 FROM agent_endpoint_profiles WHERE owner_id = ? AND id = ?", params![o.id, saved.spec.id], |r| r.get::<_, i64>(0))
            .optional()?
            .is_some();
        let count: i64 = c.query_row("SELECT COUNT(*) FROM agent_endpoint_profiles WHERE owner_id = ?", [&o.id], |r| r.get(0))?;
        if !exists && count as usize >= endpoint_limits::ENDPOINTS {
            return Err(Error::Invalid(format!("an owner keeps at most {} endpoints", endpoint_limits::ENDPOINTS)));
        }
        if saved.sandbox_default {
            let others: Vec<(String, String)> = c.query(
                "SELECT id, json FROM agent_endpoint_profiles WHERE owner_id = ? AND id <> ?",
                params![o.id, saved.spec.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            for (other, json) in others {
                if let Some(mut p) = parse(&json).filter(|p| p.sandbox_default) {
                    p.sandbox_default = false;
                    let json = serde_json::to_string(&p).map_err(|e| Error::Internal(e.into()))?;
                    c.execute("UPDATE agent_endpoint_profiles SET json = ? WHERE owner_id = ? AND id = ?", params![json, o.id, other])?;
                }
            }
        }
        let json = serde_json::to_string(&saved).map_err(|e| Error::Internal(e.into()))?;
        c.execute(
            "INSERT INTO agent_endpoint_profiles (owner_id, id, json, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (owner_id, id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
            params![o.id, saved.spec.id, json, saved.updated_at as i64],
        )?;
        Ok(())
    })?;
    Ok(Json(profile))
}

pub(super) async fn delete_profile(State(code): State<Arc<Code>>, caller: Caller, Path((owner, id)): Path<(String, String)>) -> Result<StatusCode> {
    let o = changeable(&code, &caller, &owner)?;
    let gone = code.db.sql().execute("DELETE FROM agent_endpoint_profiles WHERE owner_id = ? AND id = ?", params![o.id, id])?;
    if gone == 0 {
        return Err(Error::NotFound(format!("endpoint {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}
