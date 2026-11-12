//! `/v1/access` (docs/permissions.md, "HTTP API"): grants, approvals, the audit log and OAuth
//! clients, for the dashboard and `roll token`. The gate has already decided each request (the
//! catalogue's `access` rows); handlers only tell the operator from a grant.

use crate::store::{AuditQuery, GrantChange, NewGrant};
use crate::{Access, Error, catalog};
use axum::extract::{Path, Query, State};
use axum::http::{Extensions, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use roll_auth::GrantKind;
use roll_auth::scope::{Action, Outcome, PRODUCTS, Scope, Target};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Access>>;

pub fn router(access: Arc<Access>) -> Router {
    Router::new()
        .route("/v1/access", get(me))
        .route("/v1/access/catalog", get(catalog_view))
        .route("/v1/access/tokens", get(list).post(create))
        .route("/v1/access/tokens/{id}", get(get_one).patch(update).delete(revoke))
        .route("/v1/access/tokens/{id}/rotate", post(rotate))
        .route("/v1/access/approvals", get(approvals))
        .route("/v1/access/approvals/{id}", get(approval))
        .route("/v1/access/approvals/{id}/approve", post(approve))
        .route("/v1/access/approvals/{id}/deny", post(deny))
        .route("/v1/access/audit", get(audit))
        .route("/v1/access/oauth/clients", get(clients))
        .route("/v1/access/oauth/clients/{id}", get(client))
        .route("/v1/access/oauth/authorize", post(crate::oauth::authorize))
        .with_state(access)
}

async fn blocking<T: Send + 'static>(a: Arc<Access>, f: impl FnOnce(&Access) -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(move || f(&a)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// Who's calling: the operator (`None`), or a grant.
pub(crate) enum Who {
    Operator,
    Grant(roll_auth::Grant),
}

pub(crate) fn who(access: &Access, ext: &Extensions) -> crate::Result<Who> {
    if let Some(g) = roll_auth::Grant::on(ext) {
        return Ok(Who::Grant(g.clone()));
    }
    if roll_auth::Operator::on(ext) || !access.config.locked {
        return Ok(Who::Operator);
    }
    Err(Error::Unauthorized("this API needs a token".into()))
}

fn operator_only(access: &Access, ext: &Extensions, what: &str) -> crate::Result<()> {
    match who(access, ext)? {
        Who::Operator => Ok(()),
        Who::Grant(_) => Err(Error::Forbidden(format!("only the operator can {what}"))),
    }
}

async fn me(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    Ok(Json(match who(&a, &ext)? {
        Who::Operator => json!({ "kind": "operator", "locked": a.config.locked }),
        Who::Grant(g) => {
            let id = g.id.to_string();
            let grant = blocking(a.clone(), move |a| a.store.get(&id)).await?;
            json!({ "kind": if g.kind == GrantKind::Oauth { "oauth" } else { "token" }, "grant": grant, "locked": a.config.locked })
        }
    }))
}

/// Products, actions, presets and every classified route: what the builder offers.
async fn catalog_view(State(a): S) -> Json<Value> {
    let rows: Vec<_> = catalog::rows().iter().map(catalog::view).collect();
    Json(json!({
        "products": PRODUCTS,
        "actions": Action::ALL,
        "presets": presets(),
        "routes": rows,
        "operator_only": catalog::OPERATOR_ONLY.iter().map(|(r, why)| json!({ "route": r, "why": why })).collect::<Vec<_>>(),
        "api_url": a.config.api_url,
    }))
}

/// The builder's starting points (docs/permissions.md, "Presets"). `project` in a rule's
/// resources means "the project the user picks" (every resource when none is picked).
pub fn presets() -> Value {
    let all = |actions: Value| json!({ "product": "*", "resources": "*", "actions": actions });
    json!([
        { "id": "read-only", "name": "Read only", "description": "See everything; change nothing.", "rules": [all(json!({ "read": "allow" }))] },
        {
            "id": "agent", "name": "Agent, asks before destructive",
            "description": "Read and write everywhere. Deleting, revealing credentials and shipping to production wait for your approval.",
            "rules": [all(json!({ "read": "allow", "write": "allow", "delete": "ask", "reveal": "ask", "deploy": "ask" }))],
        },
        {
            "id": "project", "name": "This project only",
            "description": "Read and write one workspace project's resources; destructive actions ask.",
            "rules": [{ "product": "*", "resources": "project", "actions": { "read": "allow", "write": "allow", "delete": "ask", "reveal": "ask", "deploy": "ask" } }],
        },
        {
            "id": "development", "name": "Dev environment only",
            "description": "Everything in development environments; revealing secrets asks. Production is out of reach.",
            "rules": [{ "product": "*", "resources": "project", "environments": ["development"], "actions": { "read": "allow", "write": "allow", "delete": "allow", "reveal": "ask" } }],
        },
        {
            "id": "previews", "name": "Previews only",
            "description": "Preview environments only: create, change and tear them down. Nothing they were branched from.",
            "rules": [{ "product": "*", "resources": "*", "environments": ["preview"], "actions": { "read": "allow", "write": "allow", "delete": "allow", "reveal": "ask" } }],
        },
        { "id": "full", "name": "Full access", "description": "Everything the operator can do, until it expires.", "rules": [all(json!({ "read": "allow", "write": "allow", "delete": "allow", "reveal": "allow", "deploy": "allow", "admin": "allow" }))] },
    ])
}

async fn list(State(a): S) -> crate::Result<Json<Value>> {
    let grants = blocking(a, |a| a.store.list()).await?;
    Ok(Json(json!(grants)))
}

async fn get_one(State(a): S, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    Ok(Json(json!(blocking(a, move |a| a.store.get(&id)).await?)))
}

async fn create(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Response> {
    let new: NewGrant = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let by = match who(&a, &ext)? {
        Who::Operator => "operator".to_string(),
        Who::Grant(g) => {
            check_within(&a, &g, &new.rules, new.expires_at, &new.ip_allow).await?;
            g.id.to_string()
        }
    };
    let (grant, token) = blocking(a, move |a| a.store.create(&new, GrantKind::Token, &by, None)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "grant": grant, "token": token }))).into_response())
}

/// A grant may hand out only what it has: rules it covers, for no longer than it lives, from no
/// more addresses than it may be used from.
async fn check_within(a: &Arc<Access>, g: &roll_auth::Grant, rules: &Scope, expires_at: Option<i64>, ip_allow: &[String]) -> crate::Result<()> {
    if !crate::may_grant(Some(g), rules) {
        return Err(Error::Forbidden("a token can only make tokens it covers: every rule must be within one of its own".into()));
    }
    let id = g.id.to_string();
    let maker = blocking(a.clone(), move |a| a.store.get(&id)).await?;
    if let Some(end) = maker.expires_at
        && expires_at.is_none_or(|e| e > end)
    {
        return Err(Error::Forbidden("a token can't make one that outlives it: set expires_at on or before its own".into()));
    }
    if !maker.ip_allow.is_empty() {
        let mine = crate::store::parse_ip_allow(&maker.ip_allow)?;
        let theirs = crate::store::parse_ip_allow(ip_allow)?;
        let inside = !theirs.is_empty() && theirs.iter().all(|t| mine.iter().any(|m| m.contains(t)));
        if !inside {
            return Err(Error::Forbidden(format!("a token limited to {} can only make tokens limited within those addresses", maker.ip_allow.join(", "))));
        }
    }
    Ok(())
}

/// The target as `g` may touch it: itself (not its addresses), or a token it covers entirely.
async fn may_touch(a: &Arc<Access>, g: &roll_auth::Grant, target: &crate::store::Grant) -> crate::Result<()> {
    if *g.id == *target.id {
        return Ok(());
    }
    check_within(a, g, &target.rules, target.expires_at, &target.ip_allow).await
}

async fn update(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    let change: GrantChange = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    if let Who::Grant(g) = who(&a, &ext)? {
        let (target_id, a2) = (id.clone(), a.clone());
        let target = blocking(a2, move |a| a.store.get(&target_id)).await?;
        // What the token is now, and what it would become, must both be within the caller's; a
        // token never widens its own addresses.
        may_touch(&a, &g, &target).await?;
        if *g.id == *target.id && change.ip_allow.as_ref().is_some_and(|ip| *ip != target.ip_allow) {
            return Err(Error::Forbidden("a token can't change the addresses it may be used from".into()));
        }
        let rules = change.rules.clone().unwrap_or(target.rules.clone());
        let expires = change.expires_at.unwrap_or(target.expires_at);
        let ips = change.ip_allow.clone().unwrap_or(target.ip_allow.clone());
        check_within(&a, &g, &rules, expires, &ips).await?;
    }
    Ok(Json(json!(blocking(a, move |a| a.store.update(&id, &change)).await?)))
}

async fn revoke(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    if let Who::Grant(g) = who(&a, &ext)? {
        let (target_id, a2) = (id.clone(), a.clone());
        let target = blocking(a2, move |a| a.store.get(&target_id)).await?;
        // A grant revokes itself, or tokens it covers.
        may_touch(&a, &g, &target).await?;
    }
    Ok(Json(json!(blocking(a, move |a| a.store.revoke(&id)).await?)))
}

/// A new secret, the same rules: shown once. A grant may rotate itself, or tokens it covers.
async fn rotate(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    if let Who::Grant(g) = who(&a, &ext)? {
        let (target_id, a2) = (id.clone(), a.clone());
        let target = blocking(a2, move |a| a.store.get(&target_id)).await?;
        may_touch(&a, &g, &target).await?;
    }
    let (grant, token) = blocking(a, move |a| a.store.rotate(&id)).await?;
    Ok(Json(json!({ "grant": grant, "token": token })))
}

#[derive(Deserialize)]
struct ApprovalsQuery {
    state: Option<String>,
    limit: Option<usize>,
}

async fn approvals(State(a): S, Query(q): Query<ApprovalsQuery>) -> crate::Result<Json<Value>> {
    let list = blocking(a, move |a| a.store.approvals(q.state.as_deref(), q.limit.unwrap_or(100))).await?;
    Ok(Json(json!(list)))
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Seconds to wait for a decision (at most 55).
    wait: Option<u64>,
}

async fn approval(State(a): S, ext: Extensions, Path(id): Path<String>, Query(q): Query<WaitQuery>) -> crate::Result<Json<Value>> {
    let who = who(&a, &ext)?;
    let (a2, id2) = (a.clone(), id.clone());
    let found = blocking(a2, move |a| a.store.approval(&id2)).await?;
    if let Who::Grant(g) = &who
        && *g.id != *found.grant_id
        && g.scope.outcome(&Target::Product { product: "access".into() }, Action::Read) != Outcome::Allow
    {
        return Err(Error::NotFound(format!("no approval {id}")));
    }
    let found = match q.wait.filter(|w| *w > 0) {
        Some(w) => crate::evaluate::wait(&a, &id, std::time::Duration::from_secs(w.min(55))).await?,
        None => found,
    };
    Ok(Json(json!(found)))
}

#[derive(Deserialize, Default)]
struct Decision {
    reason: Option<String>,
}

async fn approve(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    operator_only(&a, &ext, "approve a request")?;
    Ok(Json(json!(blocking(a, move |a| a.store.decide(&id, true, "operator", None)).await?)))
}

async fn deny(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Json<Value>> {
    operator_only(&a, &ext, "deny a request")?;
    let d: Decision = if body.iter().all(u8::is_ascii_whitespace) { Decision::default() } else { serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))? };
    let reason = d.reason.map(|r| r.chars().take(500).collect::<String>());
    Ok(Json(json!(blocking(a, move |a| a.store.decide(&id, false, "operator", reason.as_deref())).await?)))
}

async fn audit(State(a): S, Query(q): Query<AuditQuery>) -> crate::Result<Json<Value>> {
    Ok(Json(json!(blocking(a, move |a| a.store.audit_log(&q)).await?)))
}

async fn clients(State(a): S) -> crate::Result<Json<Value>> {
    // Clients with a live connection, from the grants that name them.
    let grants = blocking(a, |a| a.store.list()).await?;
    let mut seen = std::collections::BTreeMap::new();
    for g in grants.into_iter().filter(|g| g.revoked_at.is_none()) {
        if let Some(c) = g.client {
            seen.entry(c.id.clone()).or_insert(c);
        }
    }
    Ok(Json(json!(seen.into_values().collect::<Vec<_>>())))
}

async fn client(State(a): S, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    let c = blocking(a, move |a| a.store.client(&id)).await?.ok_or_else(|| Error::NotFound("no such client".into()))?;
    Ok(Json(json!(c)))
}
