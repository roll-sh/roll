//! `/v1/storage/roles`: roles for temporary credentials, and `POST …/sessions`, which mints
//! a role's credentials for a trusted server-side caller (a backend handing a browser scoped,
//! short-lived credentials, no STS client needed). See `s3_store::sts`.

use crate::api::Acct;
use crate::control::{Control, ControlError, Result};
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use s3_store::Permission;
use s3_store::sts::{self as st, Role, RoleSpec, Session, SessionRole};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

type AppState = Arc<Control>;

/// Shortest session the control plane mints (STS's minimum is 900 s; a backend handing out
/// per-upload credentials may want less).
pub const MIN_CONTROL_SESSION_S: i64 = 60;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/storage/roles", get(list).post(create))
        .route("/v1/storage/roles/{name}", get(show).patch(update).delete(remove))
        .route("/v1/storage/roles/{name}/revoke", post(revoke))
        .route("/v1/storage/roles/{name}/sessions", post(session))
        .route("/v1/storage/session-keys/rotate", post(rotate))
}

/// A JSON object body (rejections are `{"error"}` 400s).
struct Body(Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for Body {
    type Rejection = ControlError;

    async fn from_request(req: axum::extract::Request, state: &S) -> std::result::Result<Self, Self::Rejection> {
        match Json::<Value>::from_request(req, state).await {
            Ok(Json(Value::Object(m))) => Ok(Self(m)),
            Ok(_) => Err(ControlError::Invalid("the body must be a JSON object".into())),
            Err(e) => Err(ControlError::Invalid(json_error(e))),
        }
    }
}

fn json_error(e: JsonRejection) -> String {
    e.body_text()
}

fn invalid(m: impl Into<String>) -> ControlError {
    ControlError::Invalid(m.into())
}

/// A policy given as a JSON object or as JSON text, checked and minified.
fn policy_text(v: &Value, max: usize) -> Result<Option<String>> {
    let text = match v {
        Value::Null => return Ok(None),
        Value::String(s) => s.clone(),
        Value::Object(_) => v.to_string(),
        _ => return Err(invalid("policy must be an IAM policy document (a JSON object)")),
    };
    s3_server::validate_identity_policy(&text, max).map(Some).map_err(|e| invalid(format!("invalid policy: {e}")))
}

/// A role spec from a request's fields (`policy` as an object or text).
fn spec_from(mut m: Map<String, Value>) -> Result<RoleSpec> {
    let policy = match m.remove("policy") {
        Some(v) => policy_text(&v, st::MAX_ROLE_POLICY_BYTES)?,
        None => None,
    };
    let mut spec: RoleSpec = serde_json::from_value(Value::Object(m)).map_err(|e| invalid(e.to_string()))?;
    spec.policy = policy;
    Ok(spec)
}

impl Control {
    pub fn role_view(&self, r: &Role) -> Value {
        let mut v = json!(r);
        v["arn"] = json!(r.own_arn());
        v["policy"] = r.policy.as_deref().and_then(|p| serde_json::from_str(p).ok()).unwrap_or(Value::Null);
        v
    }

    fn role_in(&self, account: &str, name: &str) -> Result<Role> {
        self.store.role_in(account, name)?.ok_or_else(|| ControlError::NotFound(format!("no role {name}")))
    }

    pub async fn create_role(&self, account: &str, mut body: Map<String, Value>) -> Result<Role> {
        let name = match body.remove("name") {
            Some(Value::String(n)) => n,
            _ => return Err(invalid("name is required")),
        };
        let spec = spec_from(body)?;
        Ok(self.store.create_role_in(account, &name, spec).await?)
    }

    /// Change some of a role's fields (`null` clears `buckets` and `policy`). Its sessions
    /// keep working, with the role as it is now (as on AWS).
    pub async fn update_role(&self, account: &str, name: &str, patch: Map<String, Value>) -> Result<Role> {
        let r = self.role_in(account, name)?;
        if r.permission.is_none() {
            return Err(invalid(format!("{name} is an IAM role: change it through the IAM API")));
        }
        let mut m = Map::new();
        m.insert("description".into(), json!(r.description));
        m.insert("permission".into(), json!(r.permission));
        m.insert("buckets".into(), json!(r.buckets));
        m.insert("policy".into(), r.policy.as_deref().map_or(Value::Null, |p| Value::String(p.to_string())));
        m.insert("trust".into(), json!(r.trust));
        m.insert("max_session_s".into(), json!(r.max_session_s));
        for (k, v) in patch {
            if k == "name" {
                return Err(invalid("roles can't be renamed"));
            }
            if !m.contains_key(&k) {
                return Err(invalid(format!("unknown field {k}")));
            }
            m.insert(k, v);
        }
        let spec = spec_from(m)?;
        Ok(self.store.update_role_in(account, name, spec).await?)
    }

    /// Mint credentials of role `name` (no trust check: the control plane's callers are
    /// trusted).
    pub async fn mint(&self, account: &str, name: &str, req: SessionReq) -> Result<Value> {
        let role = self.role_in(account, name)?;
        if role.permission.is_none() && (req.permission.is_some() || req.buckets.is_some()) {
            return Err(invalid(format!("{name} is an IAM role: it has no permission level or bucket scope to narrow; narrow it with a session policy")));
        }
        let level = role.permission.unwrap_or(Permission::Read);
        let duration = req.duration_s.unwrap_or(st::ROLE_DEFAULT_SESSION_S.min(role.max_session_s));
        if !(MIN_CONTROL_SESSION_S..=role.max_session_s).contains(&duration) {
            return Err(invalid(format!("duration_s must be between {MIN_CONTROL_SESSION_S} and the role's max_session_s ({})", role.max_session_s)));
        }
        let session_name = req.session_name.unwrap_or_else(|| "roll-control-plane".into());
        st::validate_session_name(&session_name).map_err(invalid)?;
        let permission = match req.permission.as_deref() {
            None => level,
            Some(p) => {
                let p = Permission::parse(p).ok_or_else(|| invalid("permission must be read, write or admin"))?;
                if st::min_permission(p, level) != p {
                    return Err(invalid(format!("permission {} is more than the role's {}", p.as_str(), level.as_str())));
                }
                p
            }
        };
        if let Some(bs) = &req.buckets {
            for b in bs {
                s3_store::validate::bucket_name(b)?;
            }
        }
        let buckets = if role.permission.is_some() { st::narrow_buckets(role.buckets.as_deref(), req.buckets.as_deref()).map_err(invalid)? } else { Some(Vec::new()) };
        let policy = match &req.policy {
            Some(v) => policy_text(v, st::MAX_SESSION_POLICY_BYTES)?,
            None => None,
        };
        let tags: Vec<(String, String)> = req.tags.unwrap_or_default().into_iter().collect();
        let transitive = req.transitive_tag_keys.unwrap_or_default();
        st::validate_tags(&tags, &transitive).map_err(invalid)?;
        let mut s = Session::new(permission);
        s.buckets = buckets;
        s.role = Some(SessionRole { name: role.name.clone(), id: role.id.clone(), generation: role.generation });
        s.account = role.account.clone();
        s.name = Some(session_name);
        s.policy = policy;
        s.tags = tags;
        s.transitive_tag_keys = transitive;
        if let Some(si) = req.source_identity {
            st::validate_source_identity(&si).map_err(invalid)?;
            s.source_identity = Some(si);
        }
        let (s, c) = self.store.mint_session(s, duration).await?;
        let account = role.account.clone();
        let endpoint = self.cfg.endpoint.trim_end_matches('/');
        Ok(json!({
            "access_key_id": c.access_key_id,
            "secret_access_key": c.secret_access_key,
            "session_token": c.session_token,
            "expiration": c.expiration,
            "role_arn": s.role_arn(&account),
            "assumed_role_arn": s.assumed_role_arn(&account),
            "permission": s.permission,
            "buckets": s.buckets,
            "endpoint": endpoint,
            "region": self.cfg.region,
            "env": {
                "AWS_ACCESS_KEY_ID": c.access_key_id,
                "AWS_SECRET_ACCESS_KEY": c.secret_access_key,
                "AWS_SESSION_TOKEN": c.session_token,
                "AWS_ENDPOINT_URL": endpoint,
                "AWS_REGION": self.cfg.region,
            },
        }))
    }
}

/// `POST /roles/{name}/sessions`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionReq {
    pub session_name: Option<String>,
    pub duration_s: Option<i64>,
    /// Narrower than the role's.
    pub permission: Option<String>,
    /// Within the role's scope.
    pub buckets: Option<Vec<String>>,
    /// A session policy (an IAM policy document, object or text).
    pub policy: Option<Value>,
    pub tags: Option<BTreeMap<String, String>>,
    pub transitive_tag_keys: Option<Vec<String>>,
    pub source_identity: Option<String>,
}

async fn list(State(c): State<AppState>, Acct(a): Acct) -> Result<Json<Vec<Value>>> {
    Ok(Json(c.store.roles_in(&a)?.iter().map(|r| c.role_view(r)).collect()))
}

async fn create(State(c): State<AppState>, Acct(a): Acct, Body(b): Body) -> Result<Json<Value>> {
    let r = c.create_role(&a, b).await?;
    Ok(Json(c.role_view(&r)))
}

async fn show(State(c): State<AppState>, Acct(a): Acct, Path(name): Path<String>) -> Result<Json<Value>> {
    let r = c.role_in(&a, &name)?;
    Ok(Json(c.role_view(&r)))
}

async fn update(State(c): State<AppState>, Acct(a): Acct, Path(name): Path<String>, Body(b): Body) -> Result<Json<Value>> {
    let r = c.update_role(&a, &name, b).await?;
    Ok(Json(c.role_view(&r)))
}

async fn remove(State(c): State<AppState>, Acct(a): Acct, Path(name): Path<String>) -> Result<Json<Value>> {
    c.store.delete_role_in(&a, &name).await?;
    Ok(Json(json!({ "deleted": name })))
}

/// Revoke the role's sessions issued until now, as AWS does: an `AWSRevokeOlderSessions`
/// inline policy denying them (they then get `AccessDenied`).
async fn revoke(State(c): State<AppState>, Acct(a): Acct, Path(name): Path<String>) -> Result<Json<Value>> {
    let r = c.store.revoke_role_sessions_in(&a, &name).await?;
    Ok(Json(c.role_view(&r)))
}

async fn session(State(c): State<AppState>, Acct(a): Acct, Path(name): Path<String>, Body(b): Body) -> Result<Json<Value>> {
    let req: SessionReq = serde_json::from_value(Value::Object(b)).map_err(|e| invalid(e.to_string()))?;
    Ok(Json(c.mint(&a, &name, req).await?))
}

async fn rotate(State(c): State<AppState>) -> Result<Json<Value>> {
    Ok(Json(json!({ "key_id": c.store.rotate_session_key().await? })))
}
