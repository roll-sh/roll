//! SCIM 2.0 (RFC 7643 and 7644) for SSO connections (docs/accounts.md, "SCIM"): a company's
//! provider (Okta, Entra ID, JumpCloud…) creates, updates and deactivates its people in roll, so
//! someone who leaves loses roll at once, as Linear's, Vercel's and GitHub's enterprise plans do.
//!
//! - A connection's admin makes it a token (`POST /v1/accounts/sso/{id}/scim`, shown once, kept
//!   hashed); the provider sends it as `Authorization: Bearer` to `/v1/accounts/scim/v2/…`. The
//!   token names the connection: everything it does is that connection's.
//! - It reaches only people whose email is in the connection's verified domains, and never an
//!   admin (admins are managed in roll): anyone else is a 404 to it. It never changes an email.
//! - A person's roll address is their primary email (`emails`), else `userName`; the provider's
//!   `userName` (Entra's UPN, say) is kept beside it, and filters match either.
//! - Deactivating (`active: false`, or DELETE) is roll's own disable, every time it's asked (a
//!   retry finishes what a failure left): sessions end, and the person's roll code owner (its
//!   tokens, machine links) is cut off. Their own roll tokens pause with the account. SCIM
//!   reactivates only an account it deactivated itself, never one an admin disabled.
//! - Users: filters `userName eq`, `externalId eq`, `emails.value eq`. Groups are
//!   [`crate::scim_groups`], for a connection whose SCIM groups an admin turned on (other paths,
//!   and `/Groups` while they're off, are SCIM 404s).

use crate::{Accounts, Error, normalize_email, sha256_hex};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use code_sql::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub(crate) const LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
pub(crate) const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
/// Users one page answers, at most.
pub(crate) const MAX_PAGE: i64 = 200;
/// The longest name, `userName` or `externalId` roll keeps.
const MAX_TEXT: usize = 255;

pub(crate) type S = State<Arc<Accounts>>;
pub(crate) type Ip = Option<axum::Extension<roll_auth::ClientIp>>;

/// A SCIM change in the audit log, as the connection that made it (`failed`: why it didn't
/// happen, or happened only in part, as a deactivation that couldn't finish).
fn event(a: &Accounts, conn: &crate::sso::Connection, action: &str, user: Option<&str>, mut detail: Value, failed: Option<&Error>, who: &crate::audit::Who) {
    if let Some(e) = failed {
        detail["refused"] = json!(e.redirect_code());
    }
    a.record(crate::audit::AccountEvent {
        actor: format!("scim:{}", conn.id),
        action: action.to_string(),
        method: "-".into(),
        target: user.map(crate::audit::clean_id),
        ok: failed.is_none(),
        status: failed.map(crate::audit::status_of).unwrap_or(200),
        ip: who.ip,
        user_agent: who.user_agent.clone(),
        detail,
    });
}

pub(crate) fn routes() -> Router<Arc<Accounts>> {
    Router::new()
        .route("/v1/accounts/scim/v2/ServiceProviderConfig", get(service_provider_config))
        .route("/v1/accounts/scim/v2/ResourceTypes", get(resource_types))
        .route("/v1/accounts/scim/v2/Schemas", get(schemas))
        .route("/v1/accounts/scim/v2/Users", get(list_users).post(create_user))
        .route("/v1/accounts/scim/v2/Users/{id}", get(get_user).put(replace_user).patch(patch_user).delete(delete_user))
        .route("/v1/accounts/scim/v2/Groups", get(crate::scim_groups::list_groups).post(crate::scim_groups::create_group))
        .route(
            "/v1/accounts/scim/v2/Groups/{id}",
            get(crate::scim_groups::get_group).put(crate::scim_groups::replace_group).patch(crate::scim_groups::patch_group).delete(crate::scim_groups::delete_group),
        )
        // Bulk, Me, a schema by id: a SCIM 404 (an unmatched path would be the gate's 401, which
        // a provider reads as a bad token and stops syncing for).
        .route("/v1/accounts/scim/v2/{*rest}", any(not_here))
}

/// A SCIM error answer (RFC 7644 §3.12).
pub(crate) struct ScimError(pub(crate) StatusCode, pub(crate) Option<&'static str>, pub(crate) String);

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({ "schemas": [ERROR], "status": self.0.as_u16().to_string(), "detail": self.2 });
        if let Some(t) = self.1 {
            body["scimType"] = json!(t);
        }
        let unauthorized = self.0 == StatusCode::UNAUTHORIZED;
        let mut r = scim_json(self.0, body);
        if unauthorized {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"roll SCIM\""));
        }
        r
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

pub(crate) type R = std::result::Result<Response, ScimError>;

pub(crate) fn scim_json(status: StatusCode, body: Value) -> Response {
    let mut r = (status, Json(body)).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/scim+json"));
    r
}

fn invalid(m: &str) -> Error {
    Error::BadRequest(m.to_string(), None)
}

pub(crate) async fn blocking<T: Send + 'static>(a: &Arc<Accounts>, f: impl FnOnce(&Accounts) -> crate::Result<T> + Send + 'static) -> std::result::Result<T, ScimError> {
    let a = a.clone();
    Ok(tokio::task::spawn_blocking(move || f(&a)).await.map_err(|e| Error::Internal(e.to_string()))??)
}

/// The connection a request's bearer token belongs to (looked up every time; never cached).
pub(crate) fn connection_of(a: &Accounts, headers: &HeaderMap) -> crate::Result<crate::sso::Connection> {
    let unauthorized = || Error::Unauthorized("a SCIM token from roll's SSO settings, as Authorization: Bearer".into());
    let raw = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).ok_or_else(unauthorized)?;
    let (scheme, token) = raw.trim().split_once(' ').ok_or_else(unauthorized)?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || !token.starts_with("rlp_") {
        return Err(unauthorized());
    }
    let id = a.db.read(|c| c.query_row("SELECT connection_id FROM scim_tokens WHERE token_hash = ? AND leaked IS NULL", [sha256_hex(token)], |r| r.get::<_, String>(0)).optional())?.ok_or_else(unauthorized)?;
    a.sso_connections()?.into_iter().find(|c| c.id == id).ok_or_else(unauthorized)
}

/// A person the connection may see: in its verified domains, not an admin.
struct Person {
    id: String,
    email: String,
    name: String,
    disabled: bool,
    disabled_by: Option<String>,
    external_id: Option<String>,
    user_name: Option<String>,
}

/// Which people a query is about.
enum Who<'a> {
    Id(&'a str),
    /// `userName eq`: the provider's userName, or the roll address.
    UserName(&'a str),
    Email(&'a str),
    ExternalId(&'a str),
    All,
}

pub(crate) fn domain_of(email: &str) -> &str {
    email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default()
}

pub(crate) fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect::<String>().trim().to_string()
}

impl Accounts {
    /// A SCIM token for a connection (replacing its old one), answered this once.
    pub fn scim_token(&self, id: &str) -> crate::Result<String> {
        // Checksummed, as roll's other secrets: scanners find a leaked one ([`Self::revoke_leaked_scim`]).
        let token = roll_auth::credentials::new_secret("rlp_");
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

    /// A SCIM token found where it shouldn't be (roll-access's leak scanning, `Access::on_leaked`):
    /// it stops at once, and the connection says where it was found until an admin makes a new
    /// one. Says whose token it was, or `None` when no live token is it.
    pub fn revoke_leaked_scim(&self, token: &str, why: &str) -> Option<String> {
        let h = sha256_hex(token);
        let why: String = why.chars().filter(|c| !c.is_control()).take(300).collect();
        let id = self
            .db
            .write(|c| {
                let id = c.query_row("SELECT connection_id FROM scim_tokens WHERE token_hash = ? AND leaked IS NULL", [h.as_str()], |r| r.get::<_, String>(0)).optional()?;
                if id.is_some() {
                    c.execute("UPDATE scim_tokens SET leaked = ? WHERE token_hash = ?", params![why, h])?;
                }
                Ok::<_, Error>(id)
            })
            .ok()??;
        tracing::warn!(connection = %id, "accounts: a leaked SCIM token was revoked");
        let name = self.sso_connections().ok()?.into_iter().find(|c| c.id == id).map(|c| c.name).unwrap_or_else(|| id.clone());
        Some(format!("SSO connection {name}'s SCIM token"))
    }

    /// Turn a connection's SCIM off: its token stops at once.
    pub fn remove_scim_token(&self, id: &str) -> crate::Result<()> {
        match self.db.write(|c| c.execute("DELETE FROM scim_tokens WHERE connection_id = ?", [id]))? {
            0 => Err(Error::NotFound(format!("SSO connection {id} has no SCIM token"))),
            _ => Ok(()),
        }
    }

    /// The people `who` names that the connection may see, a page of them (`offset`, `limit`),
    /// and how many there are: filtered in the database, never by reading every account.
    fn scim_people(&self, conn: &crate::sso::Connection, who: Who<'_>, offset: i64, limit: i64) -> crate::Result<(i64, Vec<Person>)> {
        if conn.verified.is_empty() {
            return Ok((0, Vec::new()));
        }
        let mut wheres = vec![format!("({})", conn.verified.iter().map(|_| "u.email LIKE ?").collect::<Vec<_>>().join(" OR ")), "u.admin = 0".to_string()];
        let mut args: Vec<String> = conn.verified.iter().map(|d| format!("%@{d}")).collect();
        let (clause, values): (&str, Vec<String>) = match who {
            Who::Id(id) => ("u.id = ?", vec![id.into()]),
            Who::UserName(n) => ("(LOWER(s.user_name) = ? OR u.email = ?)", vec![n.to_ascii_lowercase(), n.to_ascii_lowercase()]),
            Who::Email(e) => ("u.email = ?", vec![e.to_ascii_lowercase()]),
            Who::ExternalId(x) => ("s.external_id = ?", vec![x.into()]),
            Who::All => ("1 = 1", vec![]),
        };
        wheres.push(clause.into());
        args.extend(values);
        // Ids and the connection go in the SQL as parameters; the LIKE patterns are built from
        // verified domains (letters, digits, dots, hyphens: no wildcards).
        let from = format!("FROM users u LEFT JOIN scim_users s ON s.user_id = u.id AND s.connection_id = ? WHERE {}", wheres.join(" AND "));
        let mut all: Vec<String> = vec![conn.id.clone()];
        all.extend(args);
        let total: i64 = self.db.read(|c| c.query_row(&format!("SELECT COUNT(*) {from}"), &all, |r| r.get(0)))?;
        let rows = self.db.read(|c| {
            c.query(
                &format!("SELECT u.id, u.email, u.name, u.disabled, u.disabled_by, s.external_id, s.user_name {from} ORDER BY u.created_at, u.id LIMIT {limit} OFFSET {offset}"),
                &all,
                |r| Ok(Person { id: r.get(0)?, email: r.get(1)?, name: r.get(2)?, disabled: r.get(3)?, disabled_by: r.get(4)?, external_id: r.get(5)?, user_name: r.get(6)? }),
            )
        })?;
        Ok((total, rows))
    }

    fn scim_person(&self, conn: &crate::sso::Connection, id: &str) -> crate::Result<Person> {
        self.scim_people(conn, Who::Id(id), 0, 1)?.1.pop().ok_or_else(|| Error::NotFound(format!("no user {id}")))
    }

    /// Apply what the provider says about a person: their name, external id, userName, active.
    fn scim_update(&self, conn: &crate::sso::Connection, id: &str, ch: Change) -> crate::Result<()> {
        let person = self.scim_person(conn, id)?;
        let name = ch.name.as_deref().map(clean).filter(|n| !n.is_empty());
        self.db.write(|c| {
            if let Some(n) = &name {
                c.execute("UPDATE users SET name = ? WHERE id = ?", params![n, person.id])?;
            }
            if ch.external_id.is_some() || ch.user_name.is_some() {
                c.execute("INSERT INTO scim_users (connection_id, user_id) VALUES (?, ?) ON CONFLICT (connection_id, user_id) DO NOTHING", params![conn.id, person.id])?;
            }
            if let Some(x) = &ch.external_id {
                c.execute("UPDATE scim_users SET external_id = ? WHERE connection_id = ? AND user_id = ?", params![x.as_deref().map(clean), conn.id, person.id])?;
            }
            if let Some(u) = &ch.user_name {
                c.execute("UPDATE scim_users SET user_name = ? WHERE connection_id = ? AND user_id = ?", params![clean(u), conn.id, person.id])?;
            }
            Ok::<_, Error>(())
        })?;
        let me = format!("scim:{}", conn.id);
        match ch.active {
            // Every time: a retry finishes cutting off what a failure left (roll code's tokens).
            Some(false) => {
                self.update_user_by(&person.id, None, Some(true), if person.disabled { person.disabled_by.as_deref().unwrap_or("admin") } else { &me })?;
                tracing::info!(user = %person.id, connection = %conn.id, "accounts: SCIM deactivated a person");
            }
            // Only a disable of its own: an admin's stays.
            Some(true) if person.disabled => {
                if person.disabled_by.as_deref() != Some(me.as_str()) {
                    return Err(Error::Forbidden("an admin disabled this account in roll: only an admin turns it back on".into(), "admin_disabled"));
                }
                self.update_user_by(&person.id, None, Some(false), &me)?;
                tracing::info!(user = %person.id, connection = %conn.id, "accounts: SCIM reactivated a person");
            }
            _ => {}
        }
        Ok(())
    }

    /// A new person, in one write: the account (never an admin), disabled or not, and what the
    /// provider calls them.
    fn scim_create(&self, conn: &crate::sso::Connection, email: &str, user_name: &str, name: Option<&str>, external_id: Option<&str>, active: bool) -> crate::Result<String> {
        let handle = self.pick_handle(None, email)?;
        let id = crate::new_id("usr");
        let now = self.db.now_ms()?;
        let name: String = name.map(clean).filter(|n| !n.is_empty()).unwrap_or_else(|| handle.clone());
        let by = format!("scim:{}", conn.id);
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM users WHERE email = ? OR handle = ?", [email, handle.as_str()], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict(format!("{email} has an account: find it with filter=userName eq \"{email}\"")));
            }
            c.execute(
                "INSERT INTO users (id, email, handle, name, admin, disabled, disabled_by, code_owner_id, invited_by, created_at) VALUES (?, ?, ?, ?, 0, ?, ?, NULL, ?, ?)",
                params![id, email, handle, name, !active, (!active).then_some(by.as_str()), by, now],
            )?;
            c.execute("INSERT INTO scim_users (connection_id, user_id, external_id, user_name) VALUES (?, ?, ?, ?)", params![conn.id, id, external_id.map(clean), clean(user_name)])?;
            Ok(())
        })?;
        tracing::info!(user = %id, connection = %conn.id, "accounts: SCIM created a person");
        Ok(id)
    }
}

/// What a PUT or PATCH changes (`None`: left as it is).
#[derive(Default)]
struct Change {
    name: Option<String>,
    external_id: Option<Option<String>>,
    user_name: Option<String>,
    active: Option<bool>,
}

fn user_json(a: &Accounts, p: &Person) -> Value {
    let mut u = json!({
        "schemas": [USER],
        "id": p.id,
        "userName": p.user_name.clone().unwrap_or_else(|| p.email.clone()),
        "displayName": p.name,
        "name": { "formatted": p.name },
        "emails": [{ "value": p.email, "primary": true, "type": "work" }],
        "active": !p.disabled,
        "meta": { "resourceType": "User", "location": location(a, &p.id) },
    });
    if let Some(x) = &p.external_id {
        u["externalId"] = json!(x);
    }
    u
}

fn location(a: &Accounts, id: &str) -> String {
    format!("{}/v1/accounts/scim/v2/Users/{id}", a.config.api_url)
}

/// A boolean as Okta (`false`) or Entra (`"False"`) sends it; anything else is a 400, never
/// silently nothing (a provider would think a person gone who isn't).
fn bool_of(v: &Value) -> crate::Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(invalid("active is true or false")),
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

/// The person's roll address: their primary (or first) email, else a `userName` that is one.
fn email_of(v: &Value) -> crate::Result<String> {
    let emails = v.get("emails").and_then(Value::as_array).cloned().unwrap_or_default();
    let primary = emails.iter().find(|e| e.get("primary").and_then(Value::as_bool) == Some(true)).or_else(|| emails.first());
    let raw = primary.and_then(|e| e.get("value")).and_then(Value::as_str).or_else(|| v.get("userName").and_then(Value::as_str)).unwrap_or_default();
    normalize_email(raw).map_err(|_| invalid("the person's work email (emails, or a userName that's an email)"))
}

async fn not_here(State(a): S, headers: HeaderMap) -> R {
    blocking(&a, move |a| connection_of(a, &headers)).await?;
    Err(ScimError(StatusCode::NOT_FOUND, None, "roll's SCIM serves Users, and Groups when the connection's SCIM groups are on (no Bulk or Me)".into()))
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
    let groups = blocking(&a, move |a| connection_of(a, &headers).map(|c| crate::scim_groups::groups_on(a, &c))).await?;
    let user = json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "User", "name": "User", "endpoint": "/Users", "schema": USER });
    let all: Vec<Value> = std::iter::once(user).chain(groups.then(crate::scim_groups::resource_type)).collect();
    Ok(scim_json(StatusCode::OK, json!({ "schemas": [LIST], "totalResults": all.len(), "startIndex": 1, "itemsPerPage": all.len(), "Resources": all })))
}

async fn schemas(State(a): S, headers: HeaderMap) -> R {
    let groups = blocking(&a, move |a| connection_of(a, &headers).map(|c| crate::scim_groups::groups_on(a, &c))).await?;
    let attr = |name: &str, ty: &str, mutability: &str| json!({ "name": name, "type": ty, "multiValued": false, "required": name == "userName", "mutability": mutability, "returned": "default", "uniqueness": if name == "userName" { "server" } else { "none" } });
    let mut emails = attr("emails", "complex", "readOnly");
    emails["multiValued"] = json!(true);
    let mut name = attr("name", "complex", "readWrite");
    name["subAttributes"] = json!([attr("formatted", "string", "readWrite"), attr("givenName", "string", "writeOnly"), attr("familyName", "string", "writeOnly")]);
    let user = json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Schema"], "id": USER, "name": "User",
        "attributes": [attr("userName", "string", "readWrite"), name, attr("displayName", "string", "readWrite"), emails, attr("active", "boolean", "readWrite"), attr("externalId", "string", "readWrite")],
    });
    let all: Vec<Value> = std::iter::once(user).chain(groups.then(crate::scim_groups::schema)).collect();
    Ok(scim_json(StatusCode::OK, json!({ "schemas": [LIST], "totalResults": all.len(), "startIndex": 1, "itemsPerPage": all.len(), "Resources": all })))
}

#[derive(Deserialize)]
struct ListQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<String>,
    count: Option<String>,
}

/// `userName eq "x"`, `externalId eq "x"` or `emails.value eq "x"` (extra spaces, `\"` inside).
fn parse_filter(f: &str) -> std::result::Result<(String, String), ScimError> {
    let bad = || ScimError(StatusCode::BAD_REQUEST, Some("invalidFilter"), "filters are userName eq \"…\", externalId eq \"…\" or emails.value eq \"…\"".into());
    let f = f.trim();
    let (attr, rest) = f.split_once(char::is_whitespace).ok_or_else(bad)?;
    let (op, value) = rest.trim_start().split_once(char::is_whitespace).ok_or_else(bad)?;
    let value = value.trim().strip_prefix('"').and_then(|v| v.strip_suffix('"')).ok_or_else(bad)?.replace("\\\"", "\"");
    if !op.eq_ignore_ascii_case("eq") || value.is_empty() || value.len() > 320 {
        return Err(bad());
    }
    match attr.to_ascii_lowercase().as_str() {
        "username" => Ok(("userName".into(), value)),
        "externalid" => Ok(("externalId".into(), value)),
        "emails.value" | "emails" => Ok(("email".into(), value)),
        _ => Err(bad()),
    }
}

async fn list_users(State(a): S, headers: HeaderMap, Query(q): Query<ListQuery>) -> R {
    // RFC 7644 §3.4.2.4: a startIndex below 1 is 1, a negative count is 0.
    let num = |s: &Option<String>| s.as_deref().and_then(|v| v.trim().parse::<i64>().ok());
    let start = num(&q.start_index).unwrap_or(1).max(1);
    let count = num(&q.count).unwrap_or(100).clamp(0, MAX_PAGE);
    let filter = q.filter.clone();
    let (total, page) = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        let filter = filter.as_deref().map(parse_filter).transpose().map_err(|e| invalid(&e.2))?;
        let who = match &filter {
            Some((k, v)) if k == "userName" => Who::UserName(v),
            Some((k, v)) if k == "externalId" => Who::ExternalId(v),
            Some((_, v)) => Who::Email(v),
            None => Who::All,
        };
        let (total, people) = a.scim_people(&conn, who, start.saturating_sub(1).min(i64::MAX / 2), count)?;
        Ok((total, people.iter().map(|p| user_json(a, p)).collect::<Vec<_>>()))
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

async fn create_user(State(a): S, ip: Ip, headers: HeaderMap, Json(v): Json<Value>) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    let (id, body) = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        let email = email_of(&v)?;
        if !conn.verified.iter().any(|d| d == domain_of(&email)) {
            return Err(invalid(&format!("{email} isn't in this connection's verified domains")));
        }
        let user_name = v.get("userName").and_then(Value::as_str).map(str::trim).filter(|u| !u.is_empty()).unwrap_or(&email).to_string();
        let active = match v.get("active") {
            None | Some(Value::Null) => true,
            Some(x) => bool_of(x)?,
        };
        let external = v.get("externalId").and_then(Value::as_str);
        let id = a
            .scim_create(&conn, &email, &user_name, name_of(&v).as_deref(), external, active)
            .inspect_err(|e| event(a, &conn, "scim.create", None, json!({ "email": email }), Some(e), &who))?;
        event(a, &conn, "scim.create", Some(&id), json!({ "active": active }), None, &who);
        Ok((id.clone(), user_json(a, &a.scim_person(&conn, &id)?)))
    })
    .await?;
    let mut r = scim_json(StatusCode::CREATED, body);
    if let Ok(loc) = HeaderValue::from_str(&location(&a, &id)) {
        r.headers_mut().insert(header::LOCATION, loc);
    }
    Ok(r)
}

async fn replace_user(State(a): S, ip: Ip, headers: HeaderMap, Path(id): Path<String>, Json(v): Json<Value>) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    let body = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        let person = a.scim_person(&conn, &id)?;
        if let Ok(e) = email_of(&v)
            && e != person.email
        {
            return Err(invalid("the email can't change over SCIM"));
        }
        let ch = Change {
            name: name_of(&v),
            external_id: v.get("externalId").map(|x| x.as_str().map(str::to_string)),
            user_name: v.get("userName").and_then(Value::as_str).map(str::to_string),
            active: match v.get("active") {
                None | Some(Value::Null) => None,
                Some(x) => Some(bool_of(x)?),
            },
        };
        let detail = json!({ "active": ch.active, "user_name": ch.user_name.is_some(), "external_id": ch.external_id.is_some() });
        a.scim_update(&conn, &id, ch).inspect_err(|e| event(a, &conn, "scim.update", Some(&id), detail.clone(), Some(e), &who))?;
        event(a, &conn, "scim.update", Some(&id), detail, None, &who);
        Ok(user_json(a, &a.scim_person(&conn, &id)?))
    })
    .await?;
    Ok(scim_json(StatusCode::OK, body))
}

async fn patch_user(State(a): S, ip: Ip, headers: HeaderMap, Path(id): Path<String>, Json(v): Json<Value>) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    let body = blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        if !v.get("schemas").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x == PATCH_OP)) {
            return Err(invalid("a PatchOp"));
        }
        let mut ch = Change::default();
        for op in v.get("Operations").and_then(Value::as_array).into_iter().flatten() {
            let kind = op.get("op").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
            let path = op.get("path").and_then(Value::as_str).map(str::to_ascii_lowercase);
            let value = op.get("value").cloned().unwrap_or(Value::Null);
            match (kind.as_str(), path.as_deref()) {
                // Removing what roll keeps nothing of (title, phone numbers…) is fine; removing
                // the external id clears it; anything else roll needs stays.
                ("remove", Some("externalid")) => ch.external_id = Some(None),
                ("remove", Some("active")) => return Err(invalid("active can't be removed: set it")),
                ("remove", _) => {}
                ("replace" | "add", Some("active")) => ch.active = Some(bool_of(&value)?),
                ("replace" | "add", Some("displayname" | "name.formatted")) => ch.name = value.as_str().map(str::to_string).or(ch.name),
                ("replace" | "add", Some("externalid")) => ch.external_id = Some(value.as_str().map(str::to_string)),
                ("replace" | "add", Some("username")) => ch.user_name = value.as_str().map(str::to_string),
                ("replace" | "add", Some(p)) if p.starts_with("emails") => return Err(invalid("the email can't change over SCIM")),
                // A path roll doesn't keep (title, name.givenName…): nothing to do.
                ("replace" | "add", Some(_)) => {}
                // No path: the value is an object of attributes.
                ("replace" | "add", None) => {
                    if let Some(x) = value.get("active") {
                        ch.active = Some(bool_of(x)?);
                    }
                    if let Some(n) = name_of(&value) {
                        ch.name = Some(n);
                    }
                    if let Some(x) = value.get("externalId") {
                        ch.external_id = Some(x.as_str().map(str::to_string));
                    }
                    if let Some(u) = value.get("userName").and_then(Value::as_str) {
                        ch.user_name = Some(u.to_string());
                    }
                }
                (other, _) => return Err(invalid(&format!("op {other:?}: add, replace or remove"))),
            }
        }
        let detail = json!({ "active": ch.active, "user_name": ch.user_name.is_some(), "external_id": ch.external_id.is_some() });
        a.scim_update(&conn, &id, ch).inspect_err(|e| event(a, &conn, "scim.update", Some(&id), detail.clone(), Some(e), &who))?;
        event(a, &conn, "scim.update", Some(&id), detail, None, &who);
        Ok(user_json(a, &a.scim_person(&conn, &id)?))
    })
    .await?;
    Ok(scim_json(StatusCode::OK, body))
}

/// Deactivate: roll keeps the account (its history), disabled, as `active: false` does.
async fn delete_user(State(a): S, ip: Ip, headers: HeaderMap, Path(id): Path<String>) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    blocking(&a, move |a| {
        let conn = connection_of(a, &headers)?;
        a.scim_update(&conn, &id, Change { active: Some(false), ..Change::default() }).inspect_err(|e| event(a, &conn, "scim.update", Some(&id), json!({ "active": false }), Some(e), &who))?;
        event(a, &conn, "scim.update", Some(&id), json!({ "active": false }), None, &who);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
