//! SCIM 2.0 (RFC 7643 and 7644) for SSO connections (docs/accounts.md, "SCIM"): a company's
//! provider (Okta, Entra ID, JumpCloud…) creates, updates and deactivates its people in roll, so
//! someone who leaves loses roll at once, as Linear's, Vercel's and GitHub's enterprise plans do.
//!
//! - A connection's admin makes it a token (`POST /v1/accounts/sso/{id}/scim`, shown once, kept
//!   hashed); the provider sends it as `Authorization: Bearer` to `/v1/accounts/scim/v2/…`. The
//!   token names the connection: everything it does is that connection's.
//! - It reaches only people whose email is in the connection's verified domains, and never an
//!   admin (admins are managed in roll): anyone else is a 404 to it. It never changes an email.
//! - Deactivating (`active: false`, or DELETE) is roll's own disable: sessions end, and the
//!   person's roll code owner (its tokens, machine links) is cut off; reactivating turns the
//!   account back on, none of that back. A new person is made never an admin.
//! - Users only: no groups (the service provider config says so), filters `userName eq` and
//!   `externalId eq`.

use crate::{Accounts, Error, normalize_email, sha256_hex};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use code_sql::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
/// Users one page answers, at most.
const MAX_PAGE: usize = 200;

type S = State<Arc<Accounts>>;

pub(crate) fn routes() -> Router<Arc<Accounts>> {
    Router::new()
        .route("/v1/accounts/scim/v2/ServiceProviderConfig", get(service_provider_config))
        .route("/v1/accounts/scim/v2/ResourceTypes", get(resource_types))
        .route("/v1/accounts/scim/v2/Schemas", get(schemas))
        .route("/v1/accounts/scim/v2/Users", get(list_users).post(create_user))
        .route("/v1/accounts/scim/v2/Users/{id}", get(get_user).put(replace_user).patch(patch_user).delete(delete_user))
}

/// A SCIM error answer (RFC 7644 §3.12).
struct ScimError(StatusCode, Option<&'static str>, String);

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({ "schemas": [ERROR], "status": self.0.as_u16().to_string(), "detail": self.2 });
        if let Some(t) = self.1 {
            body["scimType"] = json!(t);
        }
        scim_json(self.0, body)
    }
}

impl From<Error> for ScimError {
    fn from(e: Error) -> ScimError {
        match e {
            Error::NotFound(m) => ScimError(StatusCode::NOT_FOUND, None, m),
            Error::Conflict(m) => ScimError(StatusCode::CONFLICT, Some("uniqueness"), m),
            Error::BadRequest(m, _) => ScimError(StatusCode::BAD_REQUEST, Some("invalidValue"), m),
            Error::Forbidden(m, _) => ScimError(StatusCode::FORBIDDEN, None, m),
            Error::Unauthorized(m) => ScimError(StatusCode::UNAUTHORIZED, None, m),
            e => {
                tracing::warn!(error = %e, "accounts: a SCIM request failed");
                ScimError(StatusCode::INTERNAL_SERVER_ERROR, None, "internal error".into())
            }
        }
    }
}

type R = std::result::Result<Response, ScimError>;

fn scim_json(status: StatusCode, body: Value) -> Response {
    let mut r = (status, Json(body)).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/scim+json"));
    r
}

async fn blocking<T: Send + 'static>(a: &Arc<Accounts>, f: impl FnOnce(&Accounts) -> crate::Result<T> + Send + 'static) -> std::result::Result<T, ScimError> {
    let a = a.clone();
    Ok(tokio::task::spawn_blocking(move || f(&a)).await.map_err(|e| Error::Internal(e.to_string()))??)
}

/// The connection a request's bearer token belongs to (looked up every time; never cached).
fn connection_of(a: &Accounts, headers: &HeaderMap) -> crate::Result<crate::sso::Connection> {
    let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))).map(str::trim);
    let unauthorized = || Error::Unauthorized("a SCIM token from roll's SSO settings, as Authorization: Bearer".into());
    let token = token.filter(|t| t.starts_with("rlp_")).ok_or_else(unauthorized)?;
    let id = a.db.read(|c| c.query_row("SELECT connection_id FROM scim_tokens WHERE token_hash = ?", [sha256_hex(token)], |r| r.get::<_, String>(0)).optional())?.ok_or_else(unauthorized)?;
    a.sso_connections()?.into_iter().find(|c| c.id == id).ok_or_else(unauthorized)
}

/// A person the connection may see: in its verified domains, not an admin.
struct Person {
    id: String,
    email: String,
    name: String,
    disabled: bool,
    external_id: Option<String>,
}

fn domain_of(email: &str) -> &str {
    email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default()
}

impl Accounts {
    /// A SCIM token for a connection (replacing its old one), answered this once.
    pub fn scim_token(&self, id: &str) -> crate::Result<String> {
        let token = crate::new_token("rlp");
        let now = self.db.now_ms()?;
        let h = sha256_hex(&token);
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM sso_connections WHERE id = ?", [id], |_| Ok(())).optional()?.is_none() {
                return Err(Error::NotFound(format!("no SSO connection {id}")));
            }
            c.execute("DELETE FROM scim_tokens WHERE connection_id = ?", [id])?;
            c.execute("INSERT INTO scim_tokens (connection_id, token_hash, created_at) VALUES (?, ?, ?)", params![id, h, now])?;
            Ok(())
        })?;
        Ok(token)
    }

    /// Turn a connection's SCIM off: its token stops at once.
    pub fn remove_scim_token(&self, id: &str) -> crate::Result<()> {
        match self.db.write(|c| c.execute("DELETE FROM scim_tokens WHERE connection_id = ?", [id]))? {
            0 => Err(Error::NotFound(format!("SSO connection {id} has no SCIM token"))),
            _ => Ok(()),
        }
    }

    fn scim_people(&self, conn: &crate::sso::Connection, only: Option<&str>) -> crate::Result<Vec<Person>> {
        let rows: Vec<(String, String, String, bool, bool, Option<String>)> = self.db.read(|c| {
            c.query(
                "SELECT u.id, u.email, u.name, u.disabled, u.admin, s.external_id FROM users u LEFT JOIN scim_users s ON s.user_id = u.id AND s.connection_id = ? ORDER BY u.created_at, u.id",
                [conn.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
        })?;
        Ok(rows
            .into_iter()
            .filter(|(id, email, _, _, admin, _)| !admin && conn.verified.iter().any(|d| d == domain_of(email)) && only.is_none_or(|o| o == id))
            .map(|(id, email, name, disabled, _, external_id)| Person { id, email, name, disabled, external_id })
            .collect())
    }

    fn scim_person(&self, conn: &crate::sso::Connection, id: &str) -> crate::Result<Person> {
        self.scim_people(conn, Some(id))?.pop().ok_or_else(|| Error::NotFound(format!("no user {id}")))
    }

    /// Apply what the provider says about a person: their name, their external id, active.
    fn scim_update(&self, conn: &crate::sso::Connection, id: &str, name: Option<&str>, external_id: Option<Option<&str>>, active: Option<bool>) -> crate::Result<()> {
        let person = self.scim_person(conn, id)?;
        let name = name.map(|n| n.chars().filter(|c| !c.is_control()).take(100).collect::<String>()).filter(|n| !n.trim().is_empty());
        self.db.write(|c| {
            if let Some(n) = &name {
                c.execute("UPDATE users SET name = ? WHERE id = ?", params![n.trim(), person.id])?;
            }
            if let Some(x) = external_id {
                c.execute(
                    "INSERT INTO scim_users (connection_id, user_id, external_id) VALUES (?, ?, ?) ON CONFLICT (connection_id, user_id) DO UPDATE SET external_id = excluded.external_id",
                    params![conn.id, person.id, x],
                )?;
            }
            Ok::<_, Error>(())
        })?;
        if let Some(a) = active
            && a == person.disabled
        {
            self.update_user(&person.id, None, Some(!a))?;
            tracing::info!(user = %person.id, connection = %conn.id, active = a, "accounts: SCIM changed a person's access");
        }
        Ok(())
    }
}

fn user_json(a: &Accounts, p: &Person) -> Value {
    let mut u = json!({
        "schemas": [USER],
        "id": p.id,
        "userName": p.email,
        "displayName": p.name,
        "name": { "formatted": p.name },
        "emails": [{ "value": p.email, "primary": true, "type": "work" }],
        "active": !p.disabled,
        "meta": { "resourceType": "User", "location": format!("{}/v1/accounts/scim/v2/Users/{}", a.config.api_url, p.id) },
    });
    if let Some(x) = &p.external_id {
        u["externalId"] = json!(x);
    }
    u
}

/// A boolean as Okta (`false`) or Entra (`"False"`) sends it.
fn bool_of(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

/// The name a SCIM user carries: `name.formatted`, given and family names, or `displayName`.
fn name_of(v: &Value) -> Option<String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let n = v.get("name").cloned().unwrap_or(Value::Null);
    let part = |k: &str| n.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    part("formatted")
        .map(str::to_string)
        .or_else(|| match (part("givenName"), part("familyName")) {
            (Some(g), Some(f)) => Some(format!("{g} {f}")),
            (Some(x), None) | (None, Some(x)) => Some(x.to_string()),
            _ => None,
        })
        .or_else(|| s("displayName").map(str::to_string))
}

async fn service_provider_config(State(a): S, headers: HeaderMap) -> R {
    blocking(&a, move |a| connection_of(a, &headers)).await?;
    Ok(scim_json(
        StatusCode::OK,
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
            "patch": { "supported": true },
            "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
            "filter": { "supported": true, "maxResults": MAX_PAGE },
            "changePassword": { "supported": false },
            "sort": { "supported": false },
            "etag": { "supported": false },
            "authenticationSchemes": [{ "type": "oauthbearertoken", "name": "Bearer token", "description": "The SCIM token from roll's SSO settings", "primary": true }],
        }),
    ))
}

async fn resource_types(State(a): S, headers: HeaderMap) -> R {
    blocking(&a, move |a| connection_of(a, &headers)).await?;
    let user = json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "User", "name": "User", "endpoint": "/Users", "schema": USER });
    Ok(scim_json(StatusCode::OK, json!({ "schemas": [LIST], "totalResults": 1, "startIndex": 1, "itemsPerPage": 1, "Resources": [user] })))
}

async fn schemas(State(a): S, headers: HeaderMap) -> R {
    blocking(&a, move |a| connection_of(a, &headers)).await?;
    let attr = |name: &str, ty: &str, mutability: &str| json!({ "name": name, "type": ty, "multiValued": false, "required": name == "userName", "mutability": mutability, "returned": "default", "uniqueness": if name == "userName" { "server" } else { "none" } });
    let user = json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Schema"], "id": USER, "name": "User",
        "attributes": [attr("userName", "string", "immutable"), attr("displayName", "string", "readWrite"), attr("active", "boolean", "readWrite"), attr("externalId", "string", "readWrite")],
    });
    Ok(scim_json(StatusCode::OK, json!({ "schemas": [LIST], "totalResults": 1, "startIndex": 1, "itemsPerPage": 1, "Resources": [user] })))
}

#[derive(Deserialize)]
struct ListQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<usize>,
    count: Option<usize>,
}

/// `userName eq "x"` or `externalId eq "x"`: the attribute and the value.
fn parse_filter(f: &str) -> std::result::Result<(String, String), ScimError> {
    let bad = || ScimError(StatusCode::BAD_REQUEST, Some("invalidFilter"), "filters are userName eq \"…\" or externalId eq \"…\"".into());
    let mut it = f.trim().splitn(3, ' ');
    let (attr, op, value) = (it.next().ok_or_else(bad)?, it.next().ok_or_else(bad)?, it.next().ok_or_else(bad)?);
    let value = value.trim().strip_prefix('"').and_then(|v| v.strip_suffix('"')).ok_or_else(bad)?;
    if !op.eq_ignore_ascii_case("eq") || value.contains('"') {
        return Err(bad());
    }
    match attr.to_ascii_lowercase().as_str() {
        "username" => Ok(("userName".into(), value.to_string())),
        "externalid" => Ok(("externalId".into(), value.to_string())),
        _ => Err(bad()),
    }
}

async fn list_users(State(a): S, headers: HeaderMap, Query(q): Query<ListQuery>) -> R {
    let filter = q.filter.as_deref().map(parse_filter).transpose()?;
    let (start, count) = (q.start_index.unwrap_or(1).max(1), q.count.unwrap_or(100).min(MAX_PAGE));
    let (total, page) = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        let mut people = a.scim_people(&conn, None)?;
        if let Some((attr, value)) = &filter {
            people.retain(|p| match attr.as_str() {
                "userName" => p.email.eq_ignore_ascii_case(value),
                _ => p.external_id.as_deref() == Some(value.as_str()),
            });
        }
        let total = people.len();
        let page: Vec<Value> = people.iter().skip(start - 1).take(count).map(|p| user_json(a, p)).collect();
        Ok((total, page))
    })
    .await?;
    Ok(scim_json(StatusCode::OK, json!({ "schemas": [LIST], "totalResults": total, "startIndex": start, "itemsPerPage": page.len(), "Resources": page })))
}

async fn get_user(State(a): S, headers: HeaderMap, Path(id): Path<String>) -> R {
    let body = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        Ok(user_json(a, &a.scim_person(&conn, &id)?))
    })
    .await?;
    Ok(scim_json(StatusCode::OK, body))
}

async fn create_user(State(a): S, headers: HeaderMap, Json(v): Json<Value>) -> R {
    let body = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        let raw = v.get("userName").and_then(Value::as_str).unwrap_or_default();
        let email = normalize_email(raw).map_err(|_| Error::BadRequest("userName is the person's work email".into(), None))?;
        if !conn.verified.iter().any(|d| d == domain_of(&email)) {
            return Err(Error::BadRequest(format!("{email} isn't in this connection's verified domains"), None));
        }
        if a.db.read(|c| c.query_row("SELECT 1 FROM users WHERE email = ?", [email.as_str()], |_| Ok(())).optional())?.is_some() {
            return Err(Error::Conflict(format!("{email} has an account: find it with filter=userName eq \"{email}\"")));
        }
        let name = name_of(&v);
        let user = a.provision_user(&email, name.as_deref(), None, &format!("scim:{}", conn.id))?;
        let external = v.get("externalId").and_then(Value::as_str).map(|x| x.chars().take(255).collect::<String>());
        let active = v.get("active").and_then(bool_of).unwrap_or(true);
        a.scim_update(&conn, &user, None, Some(external.as_deref()), Some(active))?;
        Ok(user_json(a, &a.scim_person(&conn, &user)?))
    })
    .await?;
    Ok(scim_json(StatusCode::CREATED, body))
}

async fn replace_user(State(a): S, headers: HeaderMap, Path(id): Path<String>, Json(v): Json<Value>) -> R {
    let body = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        let person = a.scim_person(&conn, &id)?;
        if let Some(u) = v.get("userName").and_then(Value::as_str)
            && !u.trim().eq_ignore_ascii_case(&person.email)
        {
            return Err(Error::BadRequest("userName (the email) can't change over SCIM".into(), None));
        }
        let external = v.get("externalId").map(|x| x.as_str().map(|s| s.chars().take(255).collect::<String>()));
        a.scim_update(&conn, &id, name_of(&v).as_deref(), external.as_ref().map(|x| x.as_deref()), v.get("active").and_then(bool_of))?;
        Ok(user_json(a, &a.scim_person(&conn, &id)?))
    })
    .await?;
    Ok(scim_json(StatusCode::OK, body))
}

async fn patch_user(State(a): S, headers: HeaderMap, Path(id): Path<String>, Json(v): Json<Value>) -> R {
    let body = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        if !v.get("schemas").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x == PATCH_OP)) {
            return Err(Error::BadRequest("a PatchOp".into(), None));
        }
        let (mut name, mut external, mut active) = (None, None, None);
        for op in v.get("Operations").and_then(Value::as_array).into_iter().flatten() {
            let kind = op.get("op").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
            if kind != "replace" && kind != "add" {
                return Err(Error::BadRequest(format!("op {kind:?}: replace or add"), None));
            }
            let value = op.get("value").cloned().unwrap_or(Value::Null);
            match op.get("path").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
                Some("active") => active = bool_of(&value).or(active),
                Some("displayname") | Some("name.formatted") => name = value.as_str().map(str::to_string).or(name),
                Some("externalid") => external = Some(value.as_str().map(str::to_string)),
                Some("username") => return Err(Error::BadRequest("userName (the email) can't change over SCIM".into(), None)),
                // A path roll doesn't keep (title, phone numbers…): nothing to do.
                Some(_) => {}
                // No path: the value is an object of attributes.
                None => {
                    if let Some(b) = value.get("active").and_then(bool_of) {
                        active = Some(b);
                    }
                    if let Some(n) = name_of(&value) {
                        name = Some(n);
                    }
                    if let Some(x) = value.get("externalId") {
                        external = Some(x.as_str().map(str::to_string));
                    }
                }
            }
        }
        let external = external.map(|x| x.map(|s| s.chars().take(255).collect::<String>()));
        a.scim_update(&conn, &id, name.as_deref(), external.as_ref().map(|x| x.as_deref()), active)?;
        Ok(user_json(a, &a.scim_person(&conn, &id)?))
    })
    .await?;
    Ok(scim_json(StatusCode::OK, body))
}

/// Deactivate: roll keeps the account (its history), disabled, as `active: false` does.
async fn delete_user(State(a): S, headers: HeaderMap, Path(id): Path<String>) -> R {
    blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        a.scim_update(&conn, &id, None, None, Some(false))
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
