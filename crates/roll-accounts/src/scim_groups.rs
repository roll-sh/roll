//! SCIM `Groups` (RFC 7643 §4.2, RFC 7644; docs/members-v1-1.md, "SCIM Groups"): a company's
//! identity provider keeps a group's members, and roll gives the group its roles on projects.
//!
//! - Off for each SSO connection until an admin turns it on (`groups`, D18): until then `/Groups`
//!   is a SCIM 404, as before, and nothing is stored.
//! - Each SCIM group is one roll group of the connection (`scim:<connection>`), made by the
//!   provider's `POST`. roll never links one to an existing group by name.
//! - The provider names members by the `usr_…` ids `/Users` gave it. Someone the connection doesn't
//!   reach (outside its verified domains, an admin, an unknown id) is skipped and counted, never an
//!   error: one admin in an IdP group mustn't stop the group syncing, and a 404 per person would let
//!   the provider probe who has an account.
//! - Groups themselves live in the workspace (rolld sets a [`Directory`]): every change is one of
//!   its writes, and taking someone out takes effect on their next request.
//! - Bounds: a body is read up to [`BODY_MAX`] (a bigger one is a 413 and nothing is parsed); at
//!   most [`MEMBERS_PER_REQUEST`] member values of at most 64 bytes, [`OPS_MAX`] operations and 256
//!   bytes a path. Pages are clamped to 200 and filtered in SQL.

use crate::scim::{LIST, MAX_PAGE, PATCH_OP, R, S, ScimError, blocking, clean, connection_of, domain_of, scim_json};
use crate::{Accounts, Error};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
/// The most bytes of a `/Groups` body read.
pub const BODY_MAX: usize = 1 << 20;
/// The most member values one `POST`, `PUT` or `PATCH` value carries.
pub const MEMBERS_PER_REQUEST: usize = 1_000;
/// The most operations one `PATCH` holds.
pub const OPS_MAX: usize = 100;
const VALUE_MAX: usize = 64;
const PATH_MAX: usize = 256;

/// What SCIM changes in roll's groups (rolld: the workspace). Every call is one write; every id it
/// takes is a person the connection reaches (this side checked).
pub trait Directory: Send + Sync {
    /// The connection's groups, filtered (`("displayName" | "externalId" | "id", value)`) and
    /// paged in the database, and how many there are.
    fn list(&self, conn: &str, filter: Option<(&str, &str)>, offset: i64, count: i64) -> Result<(i64, Vec<DirGroup>), DirError>;
    /// One group, with its members unless `members` is false.
    fn get(&self, conn: &str, id: &str, members: bool) -> Result<DirGroup, DirError>;
    fn create(&self, conn: &str, name: &str, external_id: Option<&str>, members: &[String]) -> Result<(DirGroup, Counts), DirError>;
    /// `ops` in order, in one write.
    fn patch(&self, conn: &str, id: &str, ops: &[GroupOp]) -> Result<Counts, DirError>;
    fn delete(&self, conn: &str, id: &str) -> Result<(), DirError>;
    /// The connection was removed: its groups go.
    fn forget_connection(&self, conn: &str) -> Result<(), DirError>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirGroup {
    /// `grp_…`
    pub id: String,
    pub name: String,
    pub external_id: Option<String>,
    /// `(usr_…, handle)`, when asked for.
    pub members: Vec<(String, String)>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroupOp {
    Add(Vec<String>),
    Remove(Vec<String>),
    /// The members are exactly these.
    Replace(Vec<String>),
    Rename(String),
    ExternalId(Option<String>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    pub removed: usize,
    pub skipped: usize,
}

#[derive(Debug)]
pub enum DirError {
    NotFound(String),
    Conflict(String),
    /// Past a group's size: nothing in the request applied.
    TooMany(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for DirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirError::NotFound(m) | DirError::Conflict(m) | DirError::TooMany(m) | DirError::Invalid(m) | DirError::Internal(m) => f.write_str(m),
        }
    }
}

impl From<DirError> for ScimError {
    fn from(e: DirError) -> ScimError {
        match e {
            DirError::NotFound(m) => ScimError(StatusCode::NOT_FOUND, None, m),
            DirError::Conflict(m) => ScimError(StatusCode::CONFLICT, Some("uniqueness"), m),
            DirError::TooMany(m) => ScimError(StatusCode::BAD_REQUEST, Some("tooMany"), m),
            DirError::Invalid(m) => ScimError(StatusCode::BAD_REQUEST, Some("invalidValue"), m),
            DirError::Internal(m) => {
                tracing::warn!(error = %m, "accounts: a SCIM group change failed");
                ScimError(StatusCode::INTERNAL_SERVER_ERROR, None, "internal error".into())
            }
        }
    }
}

impl Accounts {
    /// Where SCIM's groups are kept (set once, by rolld).
    pub fn set_directory(&self, d: Arc<dyn Directory>) {
        *self.directory.write().expect("directory") = Some(d);
    }

    pub(crate) fn directory(&self) -> Option<Arc<dyn Directory>> {
        self.directory.read().expect("directory").clone()
    }

    /// Of `ids`, the people the connection reaches (in its verified domains, not an admin), in
    /// the order given, each once; and how many it doesn't.
    fn reachable(&self, conn: &crate::sso::Connection, ids: &[String]) -> crate::Result<(Vec<String>, usize)> {
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<&String> = ids.iter().filter(|i| seen.insert(i.as_str())).collect();
        let mut ok = std::collections::HashSet::new();
        for chunk in ids.chunks(200) {
            let marks = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let rows: Vec<(String, String, bool)> = self.db.read(|c| c.query(&format!("SELECT id, email, admin FROM users WHERE id IN ({marks})"), chunk, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
            for (id, email, admin) in rows {
                if !admin && conn.verified.iter().any(|d| d == domain_of(&email)) {
                    ok.insert(id);
                }
            }
        }
        let reach: Vec<String> = ids.iter().filter(|i| ok.contains(i.as_str())).map(|i| i.to_string()).collect();
        let skipped = ids.len() - reach.len();
        Ok((reach, skipped))
    }
}

/// The connection behind the request, with its SCIM groups on; else a SCIM 404 (nothing stored).
fn groups_connection(a: &Accounts, headers: &HeaderMap) -> std::result::Result<(crate::sso::Connection, Arc<dyn Directory>), ScimError> {
    let conn = connection_of(a, headers)?;
    match a.directory() {
        Some(d) if conn.groups => Ok((conn, d)),
        _ => Err(ScimError(StatusCode::NOT_FOUND, None, "this connection's SCIM groups are off: an admin turns them on in roll's SSO settings".into())),
    }
}

/// A body read up to [`BODY_MAX`]: past it, a 413 and nothing parsed.
async fn json_body(body: Body) -> std::result::Result<Value, ScimError> {
    let bytes = axum::body::to_bytes(body, BODY_MAX).await.map_err(|_| ScimError(StatusCode::PAYLOAD_TOO_LARGE, Some("tooLarge"), format!("a /Groups body is at most {BODY_MAX} bytes")))?;
    serde_json::from_slice(&bytes).map_err(|_| ScimError(StatusCode::BAD_REQUEST, Some("invalidSyntax"), "the body isn't JSON".into()))
}

fn bad(m: impl Into<String>) -> ScimError {
    ScimError(StatusCode::BAD_REQUEST, Some("invalidValue"), m.into())
}

/// The member ids a value carries (`[{value}]`), bounded.
fn member_ids(v: &Value) -> std::result::Result<Vec<String>, ScimError> {
    let list = match v {
        Value::Null => return Ok(vec![]),
        Value::Array(a) => a,
        // Some providers send one member as an object.
        Value::Object(_) => std::slice::from_ref(v),
        _ => return Err(bad("members is a list of {value}")),
    };
    if list.len() > MEMBERS_PER_REQUEST {
        return Err(ScimError(StatusCode::BAD_REQUEST, Some("tooMany"), format!("at most {MEMBERS_PER_REQUEST} members in one request")));
    }
    list.iter()
        .map(|m| match m.get("value").and_then(Value::as_str) {
            Some(s) if !s.is_empty() && s.len() <= VALUE_MAX => Ok(s.to_string()),
            _ => Err(bad(format!("each member is {{value}}, a user id of at most {VALUE_MAX} bytes"))),
        })
        .collect()
}

fn name_value(v: &Value) -> std::result::Result<String, ScimError> {
    let n = v.as_str().map(clean).unwrap_or_default();
    if n.is_empty() { Err(bad("displayName is 1–255 characters")) } else { Ok(n) }
}

fn external_value(v: &Value) -> Option<String> {
    v.as_str().map(clean).filter(|x| !x.is_empty())
}

/// `members[value eq "usr_…"]` (Entra's remove path): the id.
fn member_filter(path: &str) -> Option<String> {
    let inner = path.strip_prefix("members[")?.strip_suffix(']')?.trim();
    let (attr, rest) = inner.split_once(char::is_whitespace)?;
    let (op, value) = rest.trim_start().split_once(char::is_whitespace)?;
    let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    (attr.eq_ignore_ascii_case("value") && op.eq_ignore_ascii_case("eq") && !value.is_empty() && value.len() <= VALUE_MAX).then(|| value.to_string())
}

/// A PATCH's operations, as the directory applies them (member ids not yet checked for reach).
fn parse_patch(v: &Value) -> std::result::Result<Vec<GroupOp>, ScimError> {
    if !v.get("schemas").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x == PATCH_OP)) {
        return Err(bad("a PatchOp"));
    }
    let ops = v.get("Operations").and_then(Value::as_array).ok_or_else(|| bad("Operations is a list"))?;
    if ops.len() > OPS_MAX {
        return Err(ScimError(StatusCode::BAD_REQUEST, Some("tooMany"), format!("at most {OPS_MAX} operations in one PATCH")));
    }
    let mut out = vec![];
    for op in ops {
        let kind = op.get("op").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
        let path = op.get("path").and_then(Value::as_str).map(str::trim);
        if path.is_some_and(|p| p.len() > PATH_MAX) {
            return Err(ScimError(StatusCode::BAD_REQUEST, Some("invalidPath"), format!("a path is at most {PATH_MAX} bytes")));
        }
        let lower = path.map(str::to_ascii_lowercase);
        let value = op.get("value").cloned().unwrap_or(Value::Null);
        match (kind.as_str(), lower.as_deref()) {
            ("add", Some("members")) => out.push(GroupOp::Add(member_ids(&value)?)),
            ("replace", Some("members")) => out.push(GroupOp::Replace(member_ids(&value)?)),
            // Without a value, every member goes; with one, those it names.
            ("remove", Some("members")) if value.is_null() => out.push(GroupOp::Replace(vec![])),
            ("remove", Some("members")) => out.push(GroupOp::Remove(member_ids(&value)?)),
            ("remove", Some(p)) if p.starts_with("members[") => {
                let id = member_filter(path.unwrap_or_default()).ok_or_else(|| ScimError(StatusCode::BAD_REQUEST, Some("invalidFilter"), "a member path is members[value eq \"usr_…\"]".into()))?;
                out.push(GroupOp::Remove(vec![id]));
            }
            ("replace" | "add", Some("displayname")) => out.push(GroupOp::Rename(name_value(&value)?)),
            ("replace" | "add", Some("externalid")) => out.push(GroupOp::ExternalId(external_value(&value))),
            ("remove", Some("externalid")) => out.push(GroupOp::ExternalId(None)),
            ("remove", Some("displayname")) => return Err(bad("displayName can't be removed: set it")),
            ("replace" | "add", None) => {
                let o = value.as_object().ok_or_else(|| bad("a value without a path is an object"))?;
                if let Some(n) = o.get("displayName") {
                    out.push(GroupOp::Rename(name_value(n)?));
                }
                if let Some(x) = o.get("externalId") {
                    out.push(GroupOp::ExternalId(external_value(x)));
                }
                if let Some(m) = o.get("members") {
                    let list = member_ids(m)?;
                    out.push(if kind == "add" { GroupOp::Add(list) } else { GroupOp::Replace(list) });
                }
            }
            // An attribute roll keeps nothing of: nothing to do.
            ("replace" | "add" | "remove", Some(_)) => {}
            (other, _) => return Err(bad(format!("op {other:?}: add, replace or remove"))),
        }
    }
    // Every operation's members together, bounded as one value's are.
    let members: usize = out.iter().map(|o| if let GroupOp::Add(l) | GroupOp::Remove(l) | GroupOp::Replace(l) = o { l.len() } else { 0 }).sum();
    if members > MEMBERS_PER_REQUEST {
        return Err(ScimError(StatusCode::BAD_REQUEST, Some("tooMany"), format!("at most {MEMBERS_PER_REQUEST} members in one request")));
    }
    Ok(out)
}

/// Each op's member ids narrowed to those the connection reaches; how many were skipped.
fn reach_ops(a: &Accounts, conn: &crate::sso::Connection, ops: Vec<GroupOp>) -> crate::Result<(Vec<GroupOp>, usize)> {
    let mut skipped = 0;
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        out.push(match op {
            GroupOp::Add(ids) => {
                let (ok, n) = a.reachable(conn, &ids)?;
                skipped += n;
                GroupOp::Add(ok)
            }
            GroupOp::Replace(ids) => {
                let (ok, n) = a.reachable(conn, &ids)?;
                skipped += n;
                GroupOp::Replace(ok)
            }
            // Taking out someone who isn't in it does nothing either way.
            other => other,
        });
    }
    Ok((out, skipped))
}

fn location(a: &Accounts, id: &str) -> String {
    format!("{}/v1/accounts/scim/v2/Groups/{id}", a.config.api_url)
}

fn group_json(a: &Accounts, g: &DirGroup, members: bool) -> Value {
    let mut v = json!({
        "schemas": [GROUP],
        "id": g.id,
        "displayName": g.name,
        "meta": { "resourceType": "Group", "location": location(a, &g.id) },
    });
    if let Some(x) = &g.external_id {
        v["externalId"] = json!(x);
    }
    if members {
        v["members"] = g.members.iter().map(|(id, display)| json!({ "value": id, "display": display, "$ref": format!("{}/v1/accounts/scim/v2/Users/{id}", a.config.api_url), "type": "User" })).collect();
    }
    v
}

/// A group change in roll-accounts' audit log, as the connection, with its counts.
fn record(a: &Accounts, conn: &crate::sso::Connection, action: &str, id: Option<&str>, counts: Counts, failed: Option<&ScimError>, who: &crate::audit::Who) {
    a.record(crate::audit::AccountEvent {
        actor: format!("scim:{}", conn.id),
        action: action.to_string(),
        method: "-".into(),
        target: id.map(crate::audit::clean_id),
        ok: failed.is_none(),
        status: failed.map_or(200, |e| e.0.as_u16()),
        ip: who.ip,
        user_agent: who.user_agent.clone(),
        detail: json!({ "added": counts.added, "removed": counts.removed, "skipped": counts.skipped }),
    });
}

#[derive(Deserialize)]
pub(crate) struct GroupQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<String>,
    count: Option<String>,
    #[serde(rename = "excludedAttributes")]
    excluded: Option<String>,
}

impl GroupQuery {
    fn without_members(&self) -> bool {
        self.excluded.as_deref().is_some_and(|x| x.split(',').any(|a| a.trim().eq_ignore_ascii_case("members")))
    }
}

/// `displayName eq "x"`, `externalId eq "x"` or `id eq "x"`.
fn parse_filter(f: &str) -> std::result::Result<(&'static str, String), ScimError> {
    let bad = || ScimError(StatusCode::BAD_REQUEST, Some("invalidFilter"), "filters are displayName eq \"…\", externalId eq \"…\" or id eq \"…\"".into());
    let (attr, rest) = f.trim().split_once(char::is_whitespace).ok_or_else(bad)?;
    let (op, value) = rest.trim_start().split_once(char::is_whitespace).ok_or_else(bad)?;
    let value = value.trim().strip_prefix('"').and_then(|v| v.strip_suffix('"')).ok_or_else(bad)?.replace("\\\"", "\"");
    if !op.eq_ignore_ascii_case("eq") || value.is_empty() || value.len() > 320 {
        return Err(bad());
    }
    match attr.to_ascii_lowercase().as_str() {
        "displayname" => Ok(("displayName", value)),
        "externalid" => Ok(("externalId", value)),
        "id" => Ok(("id", value)),
        _ => Err(bad()),
    }
}

pub(crate) async fn list_groups(State(a): S, headers: HeaderMap, Query(q): Query<GroupQuery>) -> R {
    let num = |s: &Option<String>| s.as_deref().and_then(|v| v.trim().parse::<i64>().ok());
    let start = num(&q.start_index).unwrap_or(1).max(1);
    let count = num(&q.count).unwrap_or(100).clamp(0, MAX_PAGE);
    let (total, page) = {
        let a = a.clone();
        tokio::task::spawn_blocking(move || -> std::result::Result<_, ScimError> {
            let (conn, d) = groups_connection(&a, &headers)?;
            let filter = q.filter.as_deref().map(parse_filter).transpose()?;
            let (total, groups) = d.list(&conn.id, filter.as_ref().map(|(k, v)| (*k, v.as_str())), start.saturating_sub(1).min(i64::MAX / 2), count)?;
            // A page never carries members: a group's are read one group at a time.
            Ok((total, groups.iter().map(|g| group_json(&a, g, false)).collect::<Vec<_>>()))
        })
        .await
        .map_err(|e| ScimError::from(Error::Internal(e.to_string())))??
    };
    Ok(scim_json(StatusCode::OK, json!({ "schemas": [LIST], "totalResults": total, "startIndex": start, "itemsPerPage": page.len(), "Resources": page })))
}

pub(crate) async fn get_group(State(a): S, headers: HeaderMap, Path(id): Path<String>, Query(q): Query<GroupQuery>) -> R {
    let members = !q.without_members();
    let a2 = a.clone();
    let body = tokio::task::spawn_blocking(move || -> std::result::Result<Value, ScimError> {
        let (conn, d) = groups_connection(&a2, &headers)?;
        Ok(group_json(&a2, &d.get(&conn.id, &id, members)?, members))
    })
    .await
    .map_err(|e| ScimError::from(Error::Internal(e.to_string())))??;
    Ok(scim_json(StatusCode::OK, body))
}

pub(crate) async fn create_group(State(a): S, ip: crate::scim::Ip, headers: HeaderMap, body: Body) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    let (conn, d) = {
        let (a, h) = (a.clone(), headers.clone());
        blocking(&a, move |a| Ok(groups_connection(a, &h))).await??
    };
    let v = json_body(body).await?;
    let name = name_value(v.get("displayName").unwrap_or(&Value::Null))?;
    let external = v.get("externalId").and_then(external_value);
    let ids = member_ids(v.get("members").unwrap_or(&Value::Null))?;
    let a2 = a.clone();
    let made = tokio::task::spawn_blocking(move || -> std::result::Result<(DirGroup, Counts), ScimError> {
        let (ok, skipped) = a2.reachable(&conn, &ids)?;
        let done = d.create(&conn.id, &name, external.as_deref(), &ok).map(|(g, c)| (g, Counts { skipped: c.skipped + skipped, ..c })).map_err(ScimError::from);
        match &done {
            Ok((g, c)) => record(&a2, &conn, "scim.group.create", Some(&g.id), *c, None, &who),
            Err(e) => record(&a2, &conn, "scim.group.create", None, Counts::default(), Some(e), &who),
        }
        let (g, c) = done?;
        Ok((d.get(&conn.id, &g.id, true)?, c))
    })
    .await
    .map_err(|e| ScimError::from(Error::Internal(e.to_string())))??;
    let mut r = scim_json(StatusCode::CREATED, group_json(&a, &made.0, true));
    if let Ok(loc) = HeaderValue::from_str(&location(&a, &made.0.id)) {
        r.headers_mut().insert(header::LOCATION, loc);
    }
    Ok(r)
}

/// `ops` (already bounded) applied to group `id` as one write, audited, answered with the group.
async fn change(a: Arc<Accounts>, headers: HeaderMap, ip: crate::scim::Ip, id: String, ops: impl FnOnce(&Value) -> std::result::Result<Vec<GroupOp>, ScimError> + Send + 'static, body: Body) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    let (conn, d) = {
        let h = headers.clone();
        blocking(&a, move |a| Ok(groups_connection(a, &h))).await??
    };
    let v = json_body(body).await?;
    let ops = ops(&v)?;
    let a2 = a.clone();
    let body = tokio::task::spawn_blocking(move || -> std::result::Result<Value, ScimError> {
        let (ops, skipped) = reach_ops(&a2, &conn, ops)?;
        let done = d.patch(&conn.id, &id, &ops).map(|c| Counts { skipped: c.skipped + skipped, ..c }).map_err(ScimError::from);
        match &done {
            Ok(c) => record(&a2, &conn, "scim.group.update", Some(&id), *c, None, &who),
            Err(e) => record(&a2, &conn, "scim.group.update", Some(&id), Counts::default(), Some(e), &who),
        }
        done?;
        Ok(group_json(&a2, &d.get(&conn.id, &id, true)?, true))
    })
    .await
    .map_err(|e| ScimError::from(Error::Internal(e.to_string())))??;
    Ok(scim_json(StatusCode::OK, body))
}

/// PUT: the name, `externalId` and members, all replaced in one write.
pub(crate) async fn replace_group(State(a): S, ip: crate::scim::Ip, headers: HeaderMap, Path(id): Path<String>, body: Body) -> R {
    let ops = |v: &Value| -> std::result::Result<Vec<GroupOp>, ScimError> {
        Ok(vec![
            GroupOp::Rename(name_value(v.get("displayName").unwrap_or(&Value::Null))?),
            GroupOp::ExternalId(v.get("externalId").and_then(external_value)),
            GroupOp::Replace(member_ids(v.get("members").unwrap_or(&Value::Null))?),
        ])
    };
    change(a, headers, ip, id, ops, body).await
}

pub(crate) async fn patch_group(State(a): S, ip: crate::scim::Ip, headers: HeaderMap, Path(id): Path<String>, body: Body) -> R {
    change(a, headers, ip, id, parse_patch, body).await
}

pub(crate) async fn delete_group(State(a): S, ip: crate::scim::Ip, headers: HeaderMap, Path(id): Path<String>) -> R {
    let who = crate::audit::Who::of(ip, &headers);
    let a2 = a.clone();
    tokio::task::spawn_blocking(move || -> std::result::Result<(), ScimError> {
        let (conn, d) = groups_connection(&a2, &headers)?;
        let done = d.delete(&conn.id, &id).map_err(ScimError::from);
        record(&a2, &conn, "scim.group.delete", Some(&id), Counts::default(), done.as_ref().err(), &who);
        done
    })
    .await
    .map_err(|e| ScimError::from(Error::Internal(e.to_string())))??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The Group resource type and schema, for connections whose SCIM groups are on.
pub(crate) fn resource_type() -> Value {
    json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "Group", "name": "Group", "endpoint": "/Groups", "schema": GROUP })
}

pub(crate) fn schema() -> Value {
    let attr = |name: &str, ty: &str, multi: bool, returned: &str| json!({ "name": name, "type": ty, "multiValued": multi, "required": name == "displayName", "mutability": "readWrite", "returned": returned, "uniqueness": if name == "displayName" { "server" } else { "none" } });
    let mut members = attr("members", "complex", true, "default");
    members["subAttributes"] = json!([attr("value", "string", false, "default"), attr("display", "string", false, "default")]);
    json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Schema"], "id": GROUP, "name": "Group", "attributes": [attr("displayName", "string", false, "default"), attr("externalId", "string", false, "default"), members] })
}

/// Whether the request's connection has SCIM groups on (the config and schemas say so).
pub(crate) fn groups_on(a: &Accounts, conn: &crate::sso::Connection) -> bool {
    conn.groups && a.directory().is_some()
}
