//! `/v1/workspace`: JSON in and out, errors as `{"error": "..."}` (docs/workspace.md).
//!
//! rolld's gate lets the operator and signed-in people in (`Access::User`) and decides a person's
//! request from their memberships' grant (docs/members.md); here a person who isn't an admin sees
//! only the projects they're a member of, and grants only what that grant covers.

use crate::{Actor, Error, Kind, Scope, Workspace};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::Arc;

type S = State<Arc<Workspace>>;

/// Who is asking, from what rolld's gate put on the request: a person (their session's grant, or
/// their token's and the session it acts within), the operator's token, or the operator.
struct Caller {
    person: Option<roll_auth::SignedIn>,
    grant: Option<roll_auth::Grant>,
    session: Option<roll_auth::Grant>,
    ip: Option<String>,
}

impl<St: Send + Sync> FromRequestParts<St> for Caller {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &St) -> Result<Self, Self::Rejection> {
        let ext = &parts.extensions;
        Ok(Caller {
            person: ext.get::<roll_auth::SignedIn>().cloned(),
            grant: roll_auth::Grant::on(ext).cloned(),
            session: roll_auth::SessionGrant::on(ext).cloned(),
            ip: ext.get::<roll_auth::ClientIp>().map(|c| c.0.to_string()),
        })
    }
}

impl Caller {
    /// The person, unless they act as the operator (an admin: D9).
    fn user(&self) -> Option<&str> {
        self.person.as_ref().filter(|p| !(p.admin && roll_auth::roles::decisions::ADMIN_IS_OPERATOR)).map(|p| p.user.as_str())
    }

    fn scope(&self) -> Scope<'_> {
        self.user().map_or(Scope::All, Scope::Member)
    }

    /// The namespace a new project gets: the person who makes it (an admin too), or the operator's.
    fn owner(&self) -> String {
        self.person.as_ref().map(|p| p.user.clone()).unwrap_or_default()
    }

    fn actor(&self) -> Actor {
        let id = match (self.user(), &self.grant) {
            (Some(u), _) => u.to_string(),
            (None, Some(g)) if g.kind != roll_auth::GrantKind::Session => format!("token:{}", g.id),
            (None, _) => self.person.as_ref().map_or_else(|| "operator".to_string(), |p| p.user.clone()),
        };
        // An admin's session acts as the operator, everything.
        let grant = self.grant.as_ref().filter(|g| self.user().is_some() || g.kind != roll_auth::GrantKind::Session).map(|g| (*g.scope).clone());
        Actor { id, user: self.user().map(str::to_string), grant, session: self.session.as_ref().map(|g| (*g.scope).clone()), ip: self.ip.clone() }
    }


    /// A person's own request, made with their session (never a token): accepting an invite.
    fn session_person(&self) -> crate::Result<&str> {
        let by_token = self.grant.as_ref().is_some_and(|g| g.kind != roll_auth::GrantKind::Session);
        match (&self.person, by_token) {
            (Some(p), false) => Ok(p.user.as_str()),
            _ => Err(Error::Forbidden("this is a signed-in person's own: sign in to roll and do it there, not with a token".into())),
        }
    }
}

pub fn router(ws: Arc<Workspace>) -> Router {
    Router::new()
        .route("/v1/workspace/projects", get(list).post(create))
        .route("/v1/workspace/projects/{id}", get(get_one).patch(update).delete(delete_project))
        .route("/v1/workspace/projects/{id}/resources/{kind}/{ref}", put(place).delete(remove))
        .route("/v1/workspace/projects/{id}/members", get(members))
        .route("/v1/workspace/projects/{id}/members/me", delete(leave))
        .route("/v1/workspace/projects/{id}/members/{user}", patch(set_role).delete(remove_member))
        .route("/v1/workspace/projects/{id}/invites", get(invites).post(invite))
        .route("/v1/workspace/projects/{id}/invites/{invite}", delete(revoke_invite))
        .route("/v1/workspace/projects/{id}/homes/{kind}/{ref}", put(set_home))
        .route("/v1/workspace/projects/{id}/groups", get(project_groups))
        .route("/v1/workspace/projects/{id}/groups/{group}", put(set_group_role).delete(remove_group_role))
        .route("/v1/workspace/groups", get(groups).post(create_group))
        .route("/v1/workspace/groups/{group}", get(group).patch(update_group).delete(delete_group))
        .route("/v1/workspace/groups/{group}/members", get(group_members))
        .route("/v1/workspace/groups/{group}/members/{user}", put(add_group_member).delete(remove_group_member))
        .route("/v1/workspace/roles", get(roles))
        .route("/v1/workspace/invites", get(my_invites))
        .route("/v1/workspace/invites/{token}/accept", post(accept))
        .route("/v1/workspace/invites/{token}/decline", post(decline))
        .with_state(ws)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, None),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, None),
            Error::Conflict(_) => (StatusCode::CONFLICT, None),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, Some("forbidden")),
            Error::LastOwner(_) => (StatusCode::CONFLICT, Some("last_owner")),
            Error::Limit(_) => (StatusCode::CONFLICT, Some("limit")),
            Error::Managed(_) => (StatusCode::FORBIDDEN, Some("managed")),
            Error::TooMany(_) => (StatusCode::BAD_REQUEST, Some("limit")),
            Error::Refused(r) => {
                let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::CONFLICT);
                return (status, Json(json!({ "error": r.message, "code": r.code }))).into_response();
            }
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
        };
        let mut body = json!({ "error": self.to_string() });
        if let Some(c) = code {
            body["code"] = json!(c);
        }
        (status, Json(body)).into_response()
    }
}

/// Run `f` off the async runtime: the store blocks on its database.
async fn blocking<T: Send + 'static>(ws: Arc<Workspace>, f: impl FnOnce(&Workspace) -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(move || f(&ws)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// The body as JSON, a 400 in the API's shape if it isn't.
fn body<T: DeserializeOwned>(b: &Bytes) -> crate::Result<T> {
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    description: String,
    icon: Option<Value>,
}

#[derive(Deserialize)]
struct PlaceBody {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InviteBody {
    email: String,
    role: String,
    #[serde(default)]
    environments: Option<Vec<String>>,
}

async fn list(State(ws): S, me: Caller) -> crate::Result<Response> {
    Ok(Json(blocking(ws, move |ws| ws.list(me.scope())).await?).into_response())
}

async fn create(State(ws): S, me: Caller, b: Bytes) -> crate::Result<Response> {
    let CreateBody { name, description, icon } = body(&b)?;
    let project = blocking(ws, move |ws| ws.create(&me.owner(), &name, &description, icon.as_ref())).await?;
    Ok((StatusCode::CREATED, Json(project)).into_response())
}

async fn get_one(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<Response> {
    Ok(Json(blocking(ws, move |ws| ws.get(me.scope(), &id)).await?).into_response())
}

fn text(o: &mut Map<String, Value>, k: &str) -> crate::Result<Option<String>> {
    match o.remove(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(Error::BadRequest(format!("{k} is a string"))),
    }
}

/// `{name?, description?, icon?}`: what's there changes; `"icon": null` goes back to the default.
async fn update(State(ws): S, me: Caller, Path(id): Path<String>, b: Bytes) -> crate::Result<Response> {
    let mut o: Map<String, Value> = body(&b)?;
    let change = crate::Change {
        name: text(&mut o, "name")?,
        description: text(&mut o, "description")?,
        icon: o.remove("icon").map(|v| if v.is_null() { None } else { Some(v) }),
    };
    if let Some(k) = o.keys().next() {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    Ok(Json(blocking(ws, move |ws| ws.update(me.scope(), &id, &change)).await?).into_response())
}

/// A product may refuse (roll secrets: `409 project_holds_secrets` until a secrets admin passes
/// `?confirm=<the project's name or id>`); the request goes to it as it came.
async fn delete_project(State(ws): S, me: Caller, Path(id): Path<String>, request: axum::http::request::Parts) -> crate::Result<StatusCode> {
    blocking(ws, move |ws| ws.delete(me.scope(), &id, &request)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn place(State(ws): S, me: Caller, Path((id, kind, r#ref)): Path<(String, String, String)>, b: Bytes) -> crate::Result<Response> {
    let kind = Kind::parse(&kind)?;
    let PlaceBody { x, y } = body(&b)?;
    Ok(Json(blocking(ws, move |ws| ws.place(me.scope(), &id, kind, &r#ref, x, y)).await?).into_response())
}

async fn remove(State(ws): S, me: Caller, Path((id, kind, r#ref)): Path<(String, String, String)>) -> crate::Result<StatusCode> {
    let kind = Kind::parse(&kind)?;
    blocking(ws, move |ws| ws.remove(me.scope(), &id, kind, &r#ref)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn members(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<Response> {
    let emails = me.actor().manages(&id);
    Ok(Json(blocking(ws, move |ws| ws.members(me.scope(), &id, emails)).await?).into_response())
}

/// `{role?, environments?}`: a missing role keeps the current one, and `"environments": null`
/// goes back to the default.
async fn set_role(State(ws): S, me: Caller, Path((id, user)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let mut o: Map<String, Value> = body(&b)?;
    let role = text(&mut o, "role")?;
    let environments: Option<Option<Vec<String>>> = match o.remove("environments") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(Some(serde_json::from_value(v).map_err(|_| Error::BadRequest("environments is a list of names".into()))?)),
    };
    if let Some(k) = o.keys().next() {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    let actor = me.actor();
    let scope_user = me.user().map(str::to_string);
    let m = blocking(ws, move |ws| {
        let current = ws.members(scope_user.as_deref().map_or(Scope::All, Scope::Member), &id, false)?.into_iter().find(|m| m.user == user).ok_or_else(|| Error::NotFound(format!("{user} isn't a member of {id}")))?;
        let role = role.unwrap_or_else(|| current.role.as_str().to_string());
        // Only a developer is narrowed: another role keeps no environments it wasn't given.
        let environments = match environments {
            Some(e) => e,
            None if role == "developer" => current.environments,
            None => None,
        };
        ws.set_role(&actor, &id, &user, &role, environments)
    })
    .await?;
    Ok(Json(m).into_response())
}

async fn remove_member(State(ws): S, me: Caller, Path((id, user)): Path<(String, String)>) -> crate::Result<StatusCode> {
    let actor = me.actor();
    blocking(ws, move |ws| ws.remove_member(&actor, &id, &user)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Leave a project: any member, as themselves.
async fn leave(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<StatusCode> {
    let Some(user) = me.user().map(str::to_string) else { return Err(Error::BadRequest("only a member leaves a project: the operator isn't one".into())) };
    let actor = me.actor();
    blocking(ws, move |ws| ws.remove_member(&actor, &id, &user)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn invites(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<Response> {
    let actor = me.actor();
    let list = blocking(ws, move |ws| ws.invites(me.scope(), &id).and_then(|l| if actor.manages(&id) { Ok(l) } else { Err(Error::Forbidden("only the project's admins see its invites".into())) })).await?;
    Ok(Json(list).into_response())
}

async fn invite(State(ws): S, me: Caller, Path(id): Path<String>, b: Bytes) -> crate::Result<Response> {
    let InviteBody { email, role, environments } = body(&b)?;
    let actor = me.actor();
    let (invite, url) = blocking(ws, move |ws| ws.invite(&actor, &id, &email, &role, environments)).await?;
    let mut v = serde_json::to_value(invite).expect("JSON");
    v["url"] = json!(url);
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

async fn revoke_invite(State(ws): S, me: Caller, Path((id, invite)): Path<(String, String)>) -> crate::Result<StatusCode> {
    let actor = me.actor();
    blocking(ws, move |ws| ws.revoke_invite(&actor, &id, &invite)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_home(State(ws): S, me: Caller, Path((id, kind, r#ref)): Path<(String, String, String)>) -> crate::Result<Response> {
    let kind = Kind::parse(&kind)?;
    let actor = me.actor();
    let r2 = r#ref.clone();
    let id2 = id.clone();
    blocking(ws, move |ws| ws.set_home(&actor, &id2, kind, &r2, false)).await?;
    Ok(Json(json!({ "kind": kind, "ref": r#ref, "project": id })).into_response())
}

/// The built-in roles and their rules (`{project}` stands for the project), for the members page
/// and the token builder.
async fn roles() -> Response {
    use roll_auth::roles::{DeveloperDefault, Membership, Role, decisions, rules};
    let roles: Vec<Value> = Role::ALL.iter().map(|r| json!({ "role": r, "rules": rules(&Membership { project: "{project}".into(), role: *r, environments: None }) })).collect();
    let default = match decisions::DEVELOPER_DEFAULT {
        DeveloperDefault::Unprotected => "unprotected",
        DeveloperDefault::All => "all",
        DeveloperDefault::None => "none",
    };
    Json(json!({ "roles": roles, "products": roll_auth::roles::MEMBER_PRODUCTS, "developer_default": default })).into_response()
}

async fn my_invites(State(ws): S, me: Caller) -> crate::Result<Response> {
    let user = me.session_person()?.to_string();
    Ok(Json(blocking(ws, move |ws| ws.my_invites(&user)).await?).into_response())
}

async fn accept(State(ws): S, me: Caller, Path(token): Path<String>) -> crate::Result<Response> {
    let user = me.session_person()?.to_string();
    let ip = me.ip.clone();
    Ok(Json(blocking(ws, move |ws| ws.accept(&user, &token, ip)).await?).into_response())
}

async fn decline(State(ws): S, me: Caller, Path(token): Path<String>) -> crate::Result<StatusCode> {
    let user = me.session_person()?.to_string();
    let ip = me.ip.clone();
    blocking(ws, move |ws| ws.decline(&user, &token, ip)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Groups (docs/members-v1-1.md): server-wide groups are the operator's and roll's admins'
// (D12; no catalogue row, so rolld's gate lets only a person's session through to here, and a
// person acting as themselves is refused below); a group's roles on a project are the project's
// admins' (catalogue rows), given only by one who is in the group (D11). ──

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupBody {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    after: String,
    limit: Option<i64>,
}

async fn groups(State(ws): S, me: Caller) -> crate::Result<Response> {
    let viewer = me.user().map(str::to_string);
    Ok(Json(blocking(ws, move |ws| ws.groups(viewer.as_deref())).await?).into_response())
}

async fn create_group(State(ws): S, me: Caller, b: Bytes) -> crate::Result<Response> {
    let GroupBody { name, description } = body(&b)?;
    let actor = me.actor();
    let g = blocking(ws, move |ws| ws.create_group(&actor, &name, &description)).await?;
    Ok((StatusCode::CREATED, Json(g)).into_response())
}

async fn group(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<Response> {
    let viewer = me.user().map(str::to_string);
    Ok(Json(blocking(ws, move |ws| ws.group(viewer.as_deref(), &id)).await?).into_response())
}

/// `{name?, description?}`.
async fn update_group(State(ws): S, me: Caller, Path(id): Path<String>, b: Bytes) -> crate::Result<Response> {
    let mut o: Map<String, Value> = body(&b)?;
    let (name, description) = (text(&mut o, "name")?, text(&mut o, "description")?);
    if let Some(k) = o.keys().next() {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    let actor = me.actor();
    Ok(Json(blocking(ws, move |ws| ws.update_group(&actor, &id, name.as_deref(), description.as_deref())).await?).into_response())
}

async fn delete_group(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<StatusCode> {
    let actor = me.actor();
    blocking(ws, move |ws| ws.delete_group(&actor, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `?after=<usr_…>&limit=` (up to 500).
async fn group_members(State(ws): S, me: Caller, Path(id): Path<String>, Query(p): Query<Page>) -> crate::Result<Response> {
    let viewer = me.user().map(str::to_string);
    Ok(Json(blocking(ws, move |ws| ws.group_members(viewer.as_deref(), &id, &p.after, p.limit.unwrap_or(crate::groups::MEMBERS_PAGE_MAX))).await?).into_response())
}

async fn add_group_member(State(ws): S, me: Caller, Path((id, user)): Path<(String, String)>) -> crate::Result<StatusCode> {
    let actor = me.actor();
    blocking(ws, move |ws| ws.add_group_member(&actor, &id, &user)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_group_member(State(ws): S, me: Caller, Path((id, user)): Path<(String, String)>) -> crate::Result<StatusCode> {
    let actor = me.actor();
    blocking(ws, move |ws| ws.remove_group_member(&actor, &id, &user)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn project_groups(State(ws): S, me: Caller, Path(id): Path<String>) -> crate::Result<Response> {
    Ok(Json(blocking(ws, move |ws| ws.project_groups(me.scope(), &id)).await?).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupRoleBody {
    role: String,
    #[serde(default)]
    environments: Option<Vec<String>>,
}

async fn set_group_role(State(ws): S, me: Caller, Path((id, group)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let GroupRoleBody { role, environments } = body(&b)?;
    let actor = me.actor();
    Ok(Json(blocking(ws, move |ws| ws.set_group_role(&actor, &id, &group, &role, environments)).await?).into_response())
}

async fn remove_group_role(State(ws): S, me: Caller, Path((id, group)): Path<(String, String)>) -> crate::Result<StatusCode> {
    let actor = me.actor();
    blocking(ws, move |ws| ws.remove_group_role(&actor, &id, &group)).await?;
    Ok(StatusCode::NO_CONTENT)
}
