//! MongoDB Atlas projects (docs/secrets.md, "MongoDB Atlas"), as Vault's MongoDB Atlas database
//! plugin.
//!
//! Atlas refuses `createUser`, `updateUser` and the other user commands on every cluster tier (its
//! "Unsupported Commands"), and rolls back a user change made any other way than its UI, CLI or
//! Admin API. So roll speaks the Admin API, never the wire protocol. A project registers an Atlas
//! project by its id and an organization service account (client id and secret; the secret sealed
//! with the project's data key). Managed links rotate two database users in turn (turns.rs);
//! dynamic links make a user per lease with the link's roles and Atlas's `deleteAfterDate` as a
//! backstop, deleted when the lease ends.
//!
//! Every call goes to `https://cloud.mongodb.com` through rolld's guarded client
//! ([`crate::Deliver::request`]). The service account's access token (OAuth client credentials) is
//! kept in memory for its lifetime only. Atlas applies a user change asynchronously: a new password
//! works a minute or so after the call, which the managed grace covers.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Deliver, Error, Result, Secrets, Value, audit, names};
use base64::Engine as _;
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS atlas_projects (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    group_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    client_secret BLOB NOT NULL,
    managed_users TEXT NOT NULL,
    roles TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "mongodb_atlas";

/// The only host roll calls for Atlas.
const BASE: &str = "https://cloud.mongodb.com";

/// `databaseUsers`' resource version: its only one in the Admin API's specification (the
/// 2025-03-12 release). Atlas answers 406 to a version a resource doesn't have.
const USERS_VERSION: &str = "application/vnd.atlas.2023-01-01+json";

/// `clusters/{name}`' current resource version.
const CLUSTERS_VERSION: &str = "application/vnd.atlas.2024-08-05+json";

/// `flexClusters/{name}`' current resource version (Flex clusters aren't under `clusters`).
const FLEX_VERSION: &str = "application/vnd.atlas.2024-11-13+json";

/// The most of an answer read from Atlas.
const ANSWER_MAX: usize = 256 * 1024;

/// How long past its lease Atlas keeps a dynamic user (`deleteAfterDate`).
const MARGIN_MS: i64 = 10 * 60_000;

/// How far ahead Atlas takes a `deleteAfterDate` ("within one week of the request"), less a
/// little for the two clocks.
const AHEAD_MAX_MS: i64 = 7 * 24 * 60 * 60_000 - 5 * 60_000;

/// The shortest grace a managed link may ask for: Atlas applies a new password in about a minute,
/// and until then only the old login works.
const GRACE_MIN: u64 = 5;

/// A database role a lease never gets: the whole of every cluster.
const ADMIN_ROLES: &[&str] = &["atlasAdmin"];

#[derive(Debug, Serialize, PartialEq)]
pub struct AtlasProject {
    pub id: String,
    pub name: String,
    /// The Atlas project's id (the Admin API's `groupId`).
    pub group_id: String,
    /// The service account's client id; its secret is never shown.
    pub client_id: String,
    /// The two database users managed links rotate in turn, if any.
    pub managed_users: Vec<String>,
    /// The roles dynamic users may get (`role@db` or `role@db.collection`), if any.
    pub roles: Vec<String>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAtlasProject {
    pub name: String,
    pub group_id: String,
    pub client_id: String,
    pub client_secret: Value,
    #[serde(default)]
    pub managed_users: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// Its two managed users take turns (turns.rs), named `login` in a credential's handle.
const TURNS: crate::turns::Turns = crate::turns::Turns { table: "atlas_projects", kind: KIND, key: "login" };

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:atlas:{project}:{id}:client_secret")
}

/// An Atlas project id: 24 lower-case hex digits.
fn check_group(g: &str) -> bool {
    g.len() == 24 && g.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A service account's client id (`mdb_sa_id_…`): letters, digits, `_` and `-`.
fn check_client_id(c: &str) -> bool {
    !c.is_empty() && c.len() <= 128 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A database user's name roll accepts: letters, digits and `._@+-`, 128 at most.
fn check_user(u: &str) -> bool {
    !u.is_empty() && u.len() <= 128 && u.bytes().all(|b| b.is_ascii_alphanumeric() || b"._@+-".contains(&b))
}

/// A cluster's name, as Atlas allows it.
fn check_cluster(c: &str) -> bool {
    !c.is_empty() && c.len() <= 64 && c.as_bytes()[0].is_ascii_alphanumeric() && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A role as `role@db` or `role@db.collection` (the Atlas CLI's form): its name, database and
/// collection.
fn parse_role(s: &str) -> Option<(&str, &str, Option<&str>)> {
    let word = |w: &str, extra: &[u8], max: usize| !w.is_empty() && w.len() <= max && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || extra.contains(&b));
    let (role, rest) = s.split_once('@')?;
    let (db, collection) = match rest.split_once('.') {
        Some((d, c)) => (d, Some(c)),
        None => (rest, None),
    };
    (word(role, b"", 64) && word(db, b"", 64) && collection.is_none_or(|c| word(c, b".", 120))).then_some((role, db, collection))
}

fn role_json(s: &str) -> Json {
    let (role, db, collection) = parse_role(s).unwrap_or((s, "admin", None));
    let mut r = json!({ "roleName": role, "databaseName": db });
    if let Some(c) = collection {
        r["collectionName"] = json!(c);
    }
    r
}

/// A path segment, percent-encoded past the unreserved characters.
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Atlas's error code from an answer (`errorCode`, or OAuth's `error`), never its message.
fn code(v: &Json) -> String {
    let c = v["errorCode"].as_str().or_else(|| v["error"].as_str()).unwrap_or_default();
    if !c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { c.to_string() } else { "no error code".to_string() }
}

/// A refusal: the status and Atlas's code (its message may quote the request).
fn refused(status: u16, v: &Json, what: &str) -> Error {
    let hint = match status {
        401 | 403 => " (the service account needs Project Database Access Admin on the project, and roll's egress addresses on its API access list if it has one)",
        _ => "",
    };
    Error::Link(format!("Atlas answered {status} ({}) {what}{hint}", code(v)))
}

/// A cluster's public `mongodb+srv://` string as Atlas gives it: a host name and nothing else.
fn check_srv(s: &str) -> bool {
    s.strip_prefix("mongodb+srv://").is_some_and(|h| !h.is_empty() && h.len() <= 253 && h.contains('.') && !h.starts_with(['.', '-']) && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
}

/// A password nobody chose: 32 letters and digits (the API calls the field alphanumeric, and none
/// needs escaping in a connection string).
fn password() -> String {
    crate::directories::password(crate::directories::Flavor::Openldap)
}

/// A dynamic user's name: `roll_` and its lease's id.
fn user_for(lease: &str) -> Result<String> {
    let user = format!("roll_{lease}");
    if check_user(&user) { Ok(user) } else { Err(Error::Internal(format!("lease id {lease:?}"))) }
}

/// UTC, as Atlas takes a date (`2026-11-17T12:00:00Z`).
fn utc(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second())
}

/// When Atlas deletes a dynamic user: its lease's end plus the margin, at most a week ahead.
fn delete_after(now: i64, until: i64) -> String {
    utc((until + MARGIN_MS).min(now + AHEAD_MAX_MS))
}

/// Text for a user's `description`: letters, digits and a few marks, 100 at most.
fn description(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || " _-.:#/@".contains(c) { c } else { '-' }).take(100).collect()
}

/// Access tokens by registration, each until a minute before it ends. Memory only.
#[derive(Default)]
pub(crate) struct Tokens(Mutex<HashMap<String, (Value, Instant)>>);

impl Tokens {
    fn get(&self, id: &str) -> Option<Value> {
        let now = Instant::now();
        self.0.lock().ok()?.get(id).filter(|(_, until)| *until > now).map(|(t, _)| t.clone())
    }

    fn put(&self, id: &str, token: &Value, seconds: u64) {
        let Ok(mut m) = self.0.lock() else { return };
        let now = Instant::now();
        m.retain(|_, (_, until)| *until > now);
        if seconds > 120 {
            m.insert(id.to_string(), (token.clone(), now + Duration::from_secs(seconds - 60)));
        } else {
            m.remove(id);
        }
    }
}

/// roll as an Atlas project's service account: the Admin API.
struct Client<'a> {
    deliver: &'a dyn Deliver,
    group: String,
    client_id: String,
    secret: Value,
    /// Where its token is kept, and under which registration (a registering one keeps it only
    /// while it registers).
    cache: (&'a Tokens, String),
}

impl Client<'_> {
    /// A new access token (client credentials) and how many seconds it lasts.
    fn fetch_token(&self) -> Result<(Value, u64)> {
        let pair = Zeroizing::new(format!("{}:{}", self.client_id, self.secret.expose()));
        let basic = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(pair.as_bytes()));
        let (auth, accept) = (Value::new(format!("Basic {}", basic.as_str())), Value::new("application/json"));
        let form = Value::new("grant_type=client_credentials");
        let (status, answer) = self.deliver.request("POST", &format!("{BASE}/api/oauth/token"), &[("authorization", &auth), ("accept", &accept)], Some(("application/x-www-form-urlencoded", &form)), ANSWER_MAX)?;
        let v: Json = serde_json::from_slice(&answer).unwrap_or_default();
        if status != 200 {
            let hint = if status == 400 || status == 401 { ": is the client secret right, and the service account not deleted?" } else { "" };
            return Err(Error::Link(format!("Atlas refused a token for the service account {} ({status}, {}){hint}", self.client_id, code(&v))));
        }
        let token = v["access_token"].as_str().filter(|t| !t.is_empty() && t.len() <= 8192 && t.bytes().all(|b| b.is_ascii_graphic()));
        let (Some(token), true) = (token, v["token_type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("bearer"))) else {
            return Err(Error::Link("Atlas's token answer has no bearer token".into()));
        };
        Ok((Value::new(token), v["expires_in"].as_u64().unwrap_or(3600).min(3600)))
    }

    /// The cached token, or a new one (`fresh`: the cached one was refused).
    fn token(&self, fresh: bool) -> Result<Value> {
        let (tokens, id) = &self.cache;
        if !fresh && let Some(t) = tokens.get(id) {
            return Ok(t);
        }
        let (token, seconds) = self.fetch_token()?;
        tokens.put(id, &token, seconds);
        Ok(token)
    }

    /// One Admin API call under the project: its status and answer. A body goes as the version's
    /// JSON (it may hold a password: never in an error).
    fn call(&self, method: &str, path: &str, version: &str, body: Option<&Value>) -> Result<(u16, Json)> {
        let url = format!("{BASE}/api/atlas/v2/groups/{}{path}", self.group);
        let accept = Value::new(version);
        let mut fresh = false;
        loop {
            let auth = Value::new(format!("Bearer {}", self.token(fresh)?.expose()));
            let (status, answer) = self.deliver.request(method, &url, &[("authorization", &auth), ("accept", &accept)], body.map(|b| (version, b)), ANSWER_MAX)?;
            // A cached token Atlas no longer takes (revoked, the secret rotated): once more, new.
            if status == 401 && !fresh {
                fresh = true;
                continue;
            }
            return Ok((status, serde_json::from_slice(&answer).unwrap_or_default()));
        }
    }

    /// A SCRAM user on `admin`, if it exists.
    fn user(&self, name: &str) -> Result<Option<Json>> {
        let (status, v) = self.call("GET", &format!("/databaseUsers/admin/{}", enc(name)), USERS_VERSION, None)?;
        match status {
            200 => Ok(Some(v)),
            404 => Ok(None),
            _ => Err(refused(status, &v, &format!("reading database user {name}"))),
        }
    }

    /// The project exists and the account may read its users.
    fn probe(&self) -> Result<()> {
        let (status, v) = self.call("GET", "/databaseUsers?itemsPerPage=1", USERS_VERSION, None)?;
        if status == 200 { Ok(()) } else { Err(refused(status, &v, &format!("listing project {}'s database users", self.group))) }
    }

    /// Change a user as Vault's plugin does: read it, change what's asked, send the rest back as
    /// read (its scopes and roles stay as they are).
    fn update(&self, name: &str, change: impl FnOnce(&mut Map<String, Json>)) -> Result<()> {
        let mut user = self.user(name)?.ok_or_else(|| Error::Link(format!("Atlas has no database user {name} on admin any more")))?;
        let Some(fields) = user.as_object_mut() else { return Err(Error::Link(format!("Atlas's answer for {name} isn't a user"))) };
        fields.remove("links");
        fields.insert("groupId".into(), json!(self.group));
        change(fields);
        let body = Value::new(user.to_string());
        let (status, v) = self.call("PATCH", &format!("/databaseUsers/admin/{}", enc(name)), USERS_VERSION, Some(&body))?;
        if status == 200 { Ok(()) } else { Err(refused(status, &v, &format!("updating {name}"))) }
    }

    fn set_password(&self, name: &str, pw: &str) -> Result<()> {
        self.update(name, |u| {
            u.insert("password".into(), json!(pw));
        })
    }

    fn create(&self, body: &Value, name: &str) -> Result<()> {
        let (status, v) = self.call("POST", "/databaseUsers", USERS_VERSION, Some(body))?;
        if matches!(status, 200 | 201) { Ok(()) } else { Err(refused(status, &v, &format!("creating {name}"))) }
    }

    /// Delete a user; one Atlas already removed (its `deleteAfterDate`) is done too.
    fn delete(&self, name: &str) -> Result<()> {
        let (status, v) = self.call("DELETE", &format!("/databaseUsers/admin/{}", enc(name)), USERS_VERSION, None)?;
        if matches!(status, 200 | 202 | 204 | 404) { Ok(()) } else { Err(refused(status, &v, &format!("deleting {name}"))) }
    }

    /// A cluster's public `mongodb+srv://` connection string.
    /// A cluster's public `mongodb+srv://` connection string (a dedicated cluster's, else a Flex
    /// cluster's).
    fn srv(&self, cluster: &str) -> Result<String> {
        let (mut status, mut v) = self.call("GET", &format!("/clusters/{}", enc(cluster)), CLUSTERS_VERSION, None)?;
        if matches!(status, 400 | 404) {
            let (flex, fv) = self.call("GET", &format!("/flexClusters/{}", enc(cluster)), FLEX_VERSION, None)?;
            if flex == 200 {
                (status, v) = (flex, fv);
            }
        }
        match status {
            200 => v["connectionStrings"]["standardSrv"].as_str().filter(|s| check_srv(s)).map(String::from).ok_or_else(|| Error::Link(format!("Atlas gives cluster {cluster} no public mongodb+srv:// connection string"))),
            _ => Err(refused(status, &v, &format!("reading cluster {cluster}"))),
        }
    }
}

/// What the provider needs to reach one Atlas project.
struct Opened {
    id: String,
    group: String,
    client_id: String,
    secret: Value,
    managed_users: Vec<String>,
    roles: Vec<String>,
}

impl Opened {
    fn client<'a>(&self, deliver: &'a dyn Deliver, tokens: &'a Tokens) -> Client<'a> {
        Client { deliver, group: self.group.clone(), client_id: self.client_id.clone(), secret: self.secret.clone(), cache: (tokens, self.id.clone()) }
    }
}

const FIELDS: &[Field] = &[
    Field { field: "username", label: "Username", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "MONGODB_USERNAME", credential: Some("login") },
    Field { field: "password", label: "Password", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "MONGODB_PASSWORD", credential: Some("login") },
    Field { field: "connection_string", label: "Connection string (the link's cluster)", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "MONGODB_CONNECTION_STRING", credential: Some("login") },
];

/// A link's `cluster` option, checked.
fn link_cluster(link: &Link) -> Result<Option<String>> {
    match link.options.get("cluster") {
        None => Ok(None),
        Some(c) => c.as_str().filter(|c| check_cluster(c)).map(|c| Some(c.to_string())).ok_or_else(|| Error::BadRequest("cluster is an Atlas cluster's name: letters, digits and -".into())),
    }
}

/// A dynamic link's roles: its `roles` option (a subset of the project's), else all of them.
fn link_roles(link: &Link, allowed: &[String]) -> Result<Vec<String>> {
    let Some(asked) = link.options.get("roles") else { return Ok(allowed.to_vec()) };
    let list = asked.as_array().filter(|a| !a.is_empty()).ok_or_else(|| Error::BadRequest("roles is a list of role@db".into()))?;
    let mut out = Vec::new();
    for r in list {
        let r = r.as_str().ok_or_else(|| Error::BadRequest("roles is a list of role@db".into()))?;
        if !allowed.iter().any(|a| a == r) {
            return Err(Error::BadRequest(format!("{r} isn't one of the roles this Atlas project gives dynamic users ({})", allowed.join(", "))));
        }
        out.push(r.to_string());
    }
    Ok(out)
}

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub deliver: Arc<dyn Deliver>,
    pub tokens: Tokens,
}

impl Provider {
    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, String, String, Vec<u8>, String, String, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row(
                    "SELECT a.project_id, a.group_id, a.client_id, a.client_secret, a.managed_users, a.roles, p.dek FROM atlas_projects a JOIN projects p ON p.project_id = a.project_id WHERE a.id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
                )
                .optional()?,
            )
        })?;
        let (project, group, client_id, sealed, managed_users, roles, dek) = row.ok_or_else(|| Error::Link(format!("Atlas project {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let secret = dek.open(&aad(&project, id), &sealed)?;
        Ok(Opened { id: id.to_string(), group, client_id, secret, managed_users: serde_json::from_str(&managed_users).unwrap_or_default(), roles: serde_json::from_str(&roles).unwrap_or_default() })
    }

    /// The parts handed out: the login, its password and, with a cluster, its connection string.
    fn parts(user: String, password: Value, srv: Option<String>) -> BTreeMap<String, Value> {
        let mut parts = BTreeMap::from([("username".to_string(), Value::new(user)), ("password".to_string(), password)]);
        if let Some(s) = srv {
            parts.insert("connection_string".into(), Value::new(s));
        }
        parts
    }

    /// A user of the lease's own with the link's roles (only on the link's cluster, if it names
    /// one), which Atlas deletes after the lease even if roll doesn't.
    fn issue_dynamic(&self, r#ref: &str, link: &Link, label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        let o = self.open(r#ref)?;
        let roles = link_roles(link, &o.roles)?;
        if roles.is_empty() {
            return Err(Error::BadRequest(format!("Atlas project {ref} gives dynamic users no roles: register it with some", ref = r#ref)));
        }
        let cluster = link_cluster(link)?;
        let client = o.client(self.deliver.as_ref(), &self.tokens);
        let srv = cluster.as_deref().map(|c| client.srv(c)).transpose()?;
        let user = user_for(lease)?;
        let now = self.db.now_ms()?;
        let pw = Value::new(password());
        let mut body = json!({
            "groupId": o.group,
            "databaseName": "admin",
            "username": user,
            "password": pw.expose(),
            "roles": roles.iter().map(|r| role_json(r)).collect::<Vec<_>>(),
            "deleteAfterDate": delete_after(now, now + ttl_minutes as i64 * 60_000),
            "description": description(&format!("roll secrets lease {lease} ({label})")),
            "labels": [{ "key": "roll-secrets-lease", "value": lease }],
        });
        if let Some(c) = &cluster {
            body["scopes"] = json!([{ "name": c, "type": "CLUSTER" }]);
        }
        let body = Value::new(body.to_string());
        if let Err(why) = client.create(&body, &user) {
            let _ = client.delete(&user);
            return Err(Error::Link(format!("creating {user}: {why}")));
        }
        Ok(Issued { parts: Self::parts(user.clone(), pw, srv), handle: json!({ "user": user }) })
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("username", Mode::Managed), ("password", Mode::Managed)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM atlas_projects WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("an Atlas credential is managed or dynamic: roll keeps what it set".into()))
    }

    /// A managed link has users to rotate and a grace Atlas can meet; a dynamic one's roles are
    /// the project's; a named cluster exists and has a public connection string (asked of Atlas
    /// now, outside any transaction).
    fn check(&self, link: &Link) -> Result<()> {
        let o = self.open(&link.r#ref)?;
        let cluster = link_cluster(link)?;
        if link.field == "connection_string" && cluster.is_none() {
            return Err(Error::BadRequest("connection_string is a cluster's: give the link a cluster option".into()));
        }
        match link.mode {
            Mode::Managed if o.managed_users.is_empty() => return Err(Error::BadRequest("this Atlas project has no managed users: register it with two, or link it dynamic".into())),
            Mode::Managed if link.options.contains_key("roles") => return Err(Error::BadRequest("roles are for dynamic links".into())),
            Mode::Managed if link.option_u64("grace_minutes").is_some_and(|g| g < GRACE_MIN) => {
                return Err(Error::BadRequest(format!("Atlas applies a new password in about a minute, and until then only the old one works: grace_minutes is {GRACE_MIN} at least")));
            }
            Mode::Dynamic if o.roles.is_empty() => return Err(Error::BadRequest("this Atlas project gives dynamic users no roles: register it with some".into())),
            Mode::Dynamic => {
                link_roles(link, &o.roles)?;
            }
            _ => {}
        }
        if let Some(c) = cluster {
            o.client(self.deliver.as_ref(), &self.tokens).srv(&c).map_err(|e| Error::BadRequest(format!("roll couldn't use cluster {c}: {e}")))?;
        }
        Ok(())
    }

    /// Managed: the user not handed out last gets a new password and is handed out.
    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, label: &str, ttl: Option<u64>) -> Result<Issued> {
        if link.mode == Mode::Dynamic {
            // Never reached through a resolve (issue_lease names the user after its lease).
            return self.issue_dynamic(r#ref, link, label, ttl.unwrap_or(60), &names::new_id("lse", 16));
        }
        let o = self.open(r#ref)?;
        if o.managed_users.is_empty() {
            return Err(Error::Link(format!("Atlas project {ref} has no managed users", ref = r#ref)));
        }
        let client = o.client(self.deliver.as_ref(), &self.tokens);
        // The cluster first: a failure there changes no password.
        let srv = link_cluster(link)?.map(|c| client.srv(&c)).transpose()?;
        let (user, pw, handle) = TURNS.issue(&self.db, r#ref, &o.managed_users, password, |u, pw| client.set_password(u, pw).map_err(|e| e.to_string()))?;
        Ok(Issued { parts: Self::parts(user, pw, srv), handle })
    }

    fn issue_lease(&self, r#ref: &str, credential: &str, link: &Link, label: &str, ttl_minutes: u64, lease: &str) -> Result<Issued> {
        if link.mode != Mode::Dynamic {
            return self.issue(r#ref, credential, link, label, Some(ttl_minutes));
        }
        self.issue_dynamic(r#ref, link, label, ttl_minutes, lease)
    }

    /// A dynamic user is deleted; after its grace a managed user's password is replaced by one
    /// nobody knows, unless it was handed out again since.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let o = self.open(r#ref)?;
        let client = o.client(self.deliver.as_ref(), &self.tokens);
        if let Some(user) = handle["user"].as_str() {
            if !user.starts_with("roll_") || !check_user(user) {
                return Err(Error::Internal(format!("lease handle names {user:?}")));
            }
            return client.delete(user).map_err(|why| Error::Link(format!("deleting {user}: {why}")));
        }
        if o.managed_users.is_empty() {
            return Ok(());
        }
        TURNS.retire(&self.db, r#ref, &o.managed_users, handle, password, |u, pw| client.set_password(u, pw).map_err(|e| e.to_string()))
    }

    /// A dynamic user's `deleteAfterDate` follows its renewed lease.
    fn renew(&self, r#ref: &str, handle: &Json, until: i64) -> Result<()> {
        let Some(user) = handle["user"].as_str() else { return Ok(()) };
        let o = self.open(r#ref)?;
        let at = delete_after(self.db.now_ms()?, until);
        o.client(self.deliver.as_ref(), &self.tokens)
            .update(user, |u| {
                u.insert("deleteAfterDate".into(), json!(at));
            })
            .map_err(|why| Error::Link(format!("extending {user}: {why}")))
    }

    /// One project's managed credential at a time: two environments would hand out each other's
    /// users. (Dynamic users are each their own.)
    fn exclusive(&self) -> bool {
        true
    }
}

impl Secrets {
    pub fn atlas_projects(&self, project: &str, caller: &Caller) -> Result<Vec<AtlasProject>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, group_id, client_id, managed_users, roles, created_at, created_by FROM atlas_projects WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(AtlasProject {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    group_id: r.get(2)?,
                    client_id: r.get(3)?,
                    managed_users: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    roles: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                    created_at: r.get(6)?,
                    created_by: r.get(7)?,
                })
            })?)
        })
    }

    /// Register an Atlas project, after taking a token as its service account and finding the
    /// managed users (or, with none, reading the project's users), outside any transaction.
    pub fn add_atlas_project(&self, project: &str, n: NewAtlasProject, caller: &Caller) -> Result<AtlasProject> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let deliver = self.deliver.clone().ok_or_else(|| Error::BadRequest("MongoDB Atlas projects aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a project's name is letters, digits, - and _ (64 at most)".into()));
        }
        let group = n.group_id.trim().to_string();
        if !check_group(&group) {
            return Err(Error::BadRequest("group_id is the Atlas project's id: 24 lower-case hex digits".into()));
        }
        let client_id = n.client_id.trim().to_string();
        if !check_client_id(&client_id) {
            return Err(Error::BadRequest("client_id is the service account's client id (mdb_sa_id_…)".into()));
        }
        let secret = n.client_secret.expose().trim();
        if secret.is_empty() || secret.len() > 512 || !secret.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::BadRequest("client_secret is the service account's secret (mdb_sa_sk_…)".into()));
        }
        let secret = Value::new(secret);
        let managed_users: Vec<String> = n.managed_users.iter().map(|u| u.trim().to_string()).collect();
        if !(managed_users.is_empty() || (managed_users.len() == 2 && managed_users[0] != managed_users[1])) {
            return Err(Error::BadRequest("managed_users is two database users that take turns (or none)".into()));
        }
        if let Some(u) = managed_users.iter().find(|u| !check_user(u)) {
            return Err(Error::BadRequest(format!("{u:?} isn't a user roll manages: letters, digits and ._@+- (128 at most)")));
        }
        let mut roles: Vec<String> = Vec::new();
        for r in n.roles.iter().map(|r| r.trim()) {
            let Some((name, _, _)) = parse_role(r) else {
                return Err(Error::BadRequest(format!("{r:?} isn't a role: role@db or role@db.collection (letters, digits, _ and -)")));
            };
            if ADMIN_ROLES.contains(&name) {
                return Err(Error::BadRequest(format!("{name} administers every cluster: a lease never gets it")));
            }
            if !roles.iter().any(|x| x == r) {
                roles.push(r.to_string());
            }
        }
        if roles.len() > 16 {
            return Err(Error::BadRequest("16 roles at most".into()));
        }
        if managed_users.is_empty() && roles.is_empty() {
            return Err(Error::BadRequest("managed_users are the two users managed links rotate, roles what dynamic users get; one or both".into()));
        }
        // One call proves the secret, the project and the account's role: the managed users must
        // exist on admin (SCRAM users), or with none, the project's users must be readable.
        let held = Tokens::default();
        let client = Client { deliver: deliver.as_ref(), group: group.clone(), client_id: client_id.clone(), secret: secret.clone(), cache: (&held, String::new()) };
        let refused = |e: Error| Error::BadRequest(format!("roll couldn't use the Atlas project: {e}"));
        for u in &managed_users {
            if client.user(u).map_err(refused)?.is_none() {
                return Err(Error::BadRequest(format!("roll couldn't use the Atlas project: it has no database user {u} on admin (a password user)")));
            }
        }
        if managed_users.is_empty() {
            client.probe().map_err(refused)?;
        }
        let dek = self.project(project)?;
        let id = names::new_id("atl", 16);
        let sealed = dek.seal(&aad(project, &id), &secret);
        let now = self.db.now_ms()?;
        let a = AtlasProject { id: id.clone(), name: n.name, group_id: group, client_id, managed_users, roles, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM atlas_projects WHERE project_id = ? AND name = ?", [project, &a.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("an Atlas project named {} exists", a.name)));
            }
            // Two registrations rotating one user would undo each other's passwords.
            let others: Vec<(String, String)> = c.query("SELECT name, managed_users FROM atlas_projects WHERE project_id = ? AND group_id = ?", [project, &a.group_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            for (other, users) in others {
                let users: Vec<String> = serde_json::from_str(&users).unwrap_or_default();
                if let Some(u) = a.managed_users.iter().find(|u| users.contains(u)) {
                    return Err(Error::Conflict("exists", format!("{other} already rotates {u} in Atlas project {}", a.group_id)));
                }
            }
            c.execute(
                "INSERT INTO atlas_projects (id, project_id, name, group_id, client_id, client_secret, managed_users, roles, state, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, a.name, a.group_id, a.client_id, sealed, serde_json::to_string(&a.managed_users).expect("users"), serde_json::to_string(&a.roles).expect("roles"), crate::turns::Turns::initial(), now, a.created_by],
            )?;
            audit::record(c, project, caller, "atlas.add", None, None, &[], &format!("added MongoDB Atlas project {} ({}, service account {})", a.name, a.group_id, a.client_id), now)
        })?;
        Ok(a)
    }

    /// Delete a registration no key links to and no lease still needs.
    pub fn delete_atlas_project(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM atlas_projects WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no Atlas project {id}")))?;
            let linked: Vec<String> = c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, like.0, like.1], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let waiting: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![project, like.0, like.1], |r| r.get(0))?;
            if waiting > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} password(s) or user(s) still to retire (a grace period, or a dynamic lease): delete it once they're done")));
            }
            c.execute("DELETE FROM atlas_projects WHERE id = ?", [id])?;
            audit::record(c, project, caller, "atlas.delete", None, None, &[], &format!("deleted MongoDB Atlas project {name}"), now)
        })
    }

    pub(crate) fn atlas_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM atlas_projects WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roles_and_dates_are_checked_strictly() {
        assert!(check_group("5f1d3c2b4a6e7f8091a2b3c4") && !check_group("5F1D3C2B4A6E7F8091A2B3C4") && !check_group("5f1d/../x"));
        assert_eq!(parse_role("readWrite@shop.orders"), Some(("readWrite", "shop", Some("orders"))));
        assert_eq!(role_json("read@shop"), json!({ "roleName": "read", "databaseName": "shop" }));
        for bad in ["read", "@shop", "read@", "read@sh op", "read@shop/x", "re\"ad@shop"] {
            assert!(parse_role(bad).is_none(), "{bad}");
        }
        assert!(check_user("app-a@corp.example") && !check_user("a/b") && !check_user("a b"));
        assert_eq!(enc("a@b+c"), "a%40b%2Bc");
        assert!(check_srv("mongodb+srv://cluster0.abcde.mongodb.net") && !check_srv("mongodb+srv://u:p@evil.example") && !check_srv("mongodb://cluster0.abcde.mongodb.net"));
        assert_eq!(code(&json!({ "errorCode": "USER_ALREADY_EXISTS", "detail": "secret stuff" })), "USER_ALREADY_EXISTS");
        assert_eq!(code(&json!({ "errorCode": "has spaces: and a message" })), "no error code");
        assert_eq!(utc(1_763_380_800_000), "2025-11-17T12:00:00Z");
        // A lease's end plus the margin, never more than a week ahead.
        assert_eq!(delete_after(0, 15 * 60_000), utc(25 * 60_000));
        assert_eq!(delete_after(0, 7 * 24 * 60 * 60_000), utc(AHEAD_MAX_MS));
        assert!(password().bytes().all(|b| b.is_ascii_alphanumeric()));
    }
}
