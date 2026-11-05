//! `/v1/code/agents/{owner}/lookup/{source}/{kind}` (docs/code/http-api.md, "Lookups"): the things
//! an owner's integrations hold (Linear's teams, states, users…; Slack's channels and users;
//! Discord's channels, roles and members; GitHub's repos, branches, labels…) as rows a picker
//! shows, so nobody types an id.
//!
//! - **The owner's credentials stay here.** Each source calls its API with the install's stored
//!   token; only ids, names and pictures go to the browser.
//! - **Who.** `Code::manages`, as editing automations and the integrations is: the owner or an
//!   admin, never its agents or review apps.
//! - **Bounded.** A page is at most [`MAX_LIMIT`] rows, the search at most [`MAX_Q`] characters,
//!   `ids` at most [`MAX_IDS`]; each source reads its API's answers up to its own cap.
//! - **Short-cached per owner** ([`Cache`]): a page is served again for [`TTL`]. A write through
//!   roll (a label made here, a connection changed) drops the owner's pages of that source. A rate
//!   limit holds that source off for the owner until its retry time, serving what's cached.
//! - **Problems are quiet.** Not connected, a connection to redo, a rate limit or the platform
//!   failing come back as a 200 with `problem`, so a picker can say "Reconnect Linear" and still
//!   take a typed value. Unknown sources or kinds are 404s, a missing `parent` a 400.

mod discord;
mod linear;
mod rollcode;
mod slack;

use crate::agents::api::manageable;
use crate::api::{Body, Q};
use crate::{Caller, Code, Error, Result};
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

type S = State<Arc<Code>>;

/// Rows in a page, unless the caller asks for fewer.
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 100;
/// The longest search kept (characters).
pub const MAX_Q: usize = 100;
/// Values resolved at once (a field's chosen ids, to show their names).
pub const MAX_IDS: usize = 50;
/// How long a page is served from the cache.
pub const TTL: Duration = Duration::from_secs(30);
/// Pages kept per node.
const MAX_ENTRIES: usize = 4096;
/// The longest label, hint or id passed on.
const MAX_TEXT: usize = 300;

pub fn routes() -> Router<Arc<Code>> {
    Router::new().route("/v1/code/agents/{owner}/lookup/{source}/{kind}", get(list).post(create))
}

/// One row of a picker. `id` is what a field stores; `key` another value that names the same
/// thing (a Linear team's key, a Slack channel's name), so a field saved before pickers still
/// shows its row.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Item {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// A second, quieter line or column (an email, a team, a member count).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Rows with the same group show under one heading (a state's team).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// A picture's https URL (a user's avatar, an org's logo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// The thing's own colour (`#rrggbb`: a label, a state, a team).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// What kind of row it is when a list mixes them (`channel`, `private`, `user`, `group`, a
    /// state's type…): the picker picks an icon by it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl Item {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Item {
        Item { id: clip(id.into()), label: clip(label.into()), ..Item::default() }
    }

    pub fn key(mut self, v: Option<impl Into<String>>) -> Item {
        self.key = v.map(|v| clip(v.into())).filter(|v| !v.is_empty());
        self
    }

    pub fn hint(mut self, v: Option<impl Into<String>>) -> Item {
        self.hint = v.map(|v| clip(v.into())).filter(|v| !v.is_empty());
        self
    }

    pub fn group(mut self, v: Option<impl Into<String>>) -> Item {
        self.group = v.map(|v| clip(v.into())).filter(|v| !v.is_empty());
        self
    }

    /// Only an `https:` picture is passed on.
    pub fn avatar(mut self, v: Option<&str>) -> Item {
        self.avatar = v.filter(|u| u.starts_with("https://") && u.len() <= 2048).map(str::to_string);
        self
    }

    /// Only a `#rgb`/`#rrggbb` colour is passed on.
    pub fn color(mut self, v: Option<&str>) -> Item {
        self.color = v.filter(|c| is_color(c)).map(str::to_ascii_lowercase);
        self
    }

    pub fn icon(mut self, v: Option<impl Into<String>>) -> Item {
        self.icon = v.map(|v| clip(v.into())).filter(|v| !v.is_empty());
        self
    }
}

fn clip(s: String) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    if s.chars().count() <= MAX_TEXT { s } else { s.chars().take(MAX_TEXT).collect() }
}

fn is_color(c: &str) -> bool {
    let h = c.strip_prefix('#').unwrap_or("");
    matches!(h.len(), 3 | 6) && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why a source has no rows to show. The picker says it quietly and still takes a typed value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Problem {
    /// The owner hasn't connected the integration (or the server has no app for it).
    NotConnected { message: String },
    /// The connection must be made again (a refused token, a missing permission).
    Reconnect { message: String },
    /// The platform's rate limit: try again after `retry_after` seconds.
    RateLimited { retry_after: u64 },
    /// The platform failed or answered something roll can't read.
    Unavailable { message: String },
}

/// A page of rows. `next` asks for the following page (opaque); `problem` says why there are none.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Page {
    pub items: Vec<Item>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
}

impl Page {
    pub fn problem(p: Problem) -> Page {
        Page { problem: Some(p), ..Page::default() }
    }
}

/// What a source call can come back with besides a page: a problem with the integration (a 200
/// with `problem`) or a request roll refuses (a 4xx).
pub enum Failure {
    Problem(Problem),
    Refused(Error),
}

impl From<Problem> for Failure {
    fn from(p: Problem) -> Self {
        Failure::Problem(p)
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Refused(e)
    }
}

/// `?q=&parent=&cursor=&ids=&limit=`, checked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Query {
    /// What the person typed (trimmed, at most [`MAX_Q`] characters).
    pub q: String,
    /// The thing the rows belong to (a Linear team for its states, a repo for its branches).
    pub parent: Option<String>,
    pub cursor: Option<String>,
    /// Resolve exactly these values (ids or keys) instead of listing.
    pub ids: Vec<String>,
    pub limit: usize,
}

#[derive(Deserialize)]
struct RawQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    ids: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Query {
    fn parse(r: RawQuery) -> Result<Query> {
        let short = |what: &str, v: Option<String>, max: usize| -> Result<Option<String>> {
            match v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                Some(s) if s.len() > max => Err(Error::Invalid(format!("{what} is at most {max} bytes"))),
                v => Ok(v),
            }
        };
        let q: String = r.q.unwrap_or_default().trim().chars().filter(|c| !c.is_control()).take(MAX_Q).collect();
        let ids: Vec<String> = r.ids.unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
        if ids.len() > MAX_IDS {
            return Err(Error::Invalid(format!("ids names at most {MAX_IDS} values")));
        }
        if ids.iter().any(|i| i.len() > 200) {
            return Err(Error::Invalid("an id is at most 200 bytes".into()));
        }
        Ok(Query {
            q,
            parent: short("parent", r.parent, 200)?,
            cursor: short("cursor", r.cursor, 1000)?,
            ids,
            limit: r.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }

    /// The `parent` a kind needs.
    pub fn parent(&self, what: &str) -> std::result::Result<&str, Failure> {
        self.parent.as_deref().ok_or_else(|| Failure::Refused(Error::Invalid(format!("say which {what} (`parent`)"))))
    }
}

// ── the cache ──

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    owner: String,
    source: String,
    kind: String,
    query: Query,
}

/// Pages served lately, per owner, and the sources holding off for a rate limit.
#[derive(Default)]
pub struct Cache {
    pages: Mutex<HashMap<Key, (Instant, Page)>>,
    /// (owner, source) → until when its API isn't called.
    cooling: Mutex<HashMap<(String, String), Instant>>,
    /// (owner, source, name) → a whole list read lately ([`Known`]).
    lists: Mutex<HashMap<ListKey, (Instant, Rows)>>,
}

type ListKey = (String, String, String);
type Rows = Arc<Vec<Known>>;

/// A row of a whole list a platform can't search or page the way a picker does (Slack's channels
/// and people, a Discord server's channels and roles): read once per [`TTL`], then searched and
/// paged here. `listed: false` rows (a deactivated person, a bot) only resolve a stored value.
#[derive(Clone, Debug)]
pub struct Known {
    pub item: Item,
    pub listed: bool,
}

impl Known {
    pub fn listed(item: Item) -> Known {
        Known { item, listed: true }
    }
}

/// A page of `rows` for `q`: `ids` resolve by id or key (any row, case-insensitive, a leading
/// `#`/`@` ignored), else the listed rows whose label, key or hint holds the search, paged by an
/// offset cursor.
pub fn page_of(rows: &[Known], q: &Query) -> Page {
    if !q.ids.is_empty() {
        let bare = |v: &str| v.trim_start_matches(['#', '@']).to_lowercase();
        let wanted: Vec<String> = q.ids.iter().map(|v| bare(v)).collect();
        let hit = |i: &Item| q.ids.contains(&i.id) || i.key.as_deref().is_some_and(|k| wanted.contains(&bare(k)));
        return Page { items: rows.iter().filter(|r| hit(&r.item)).map(|r| r.item.clone()).take(MAX_IDS).collect(), ..Page::default() };
    }
    let needle = q.q.to_lowercase();
    let needle = needle.trim_start_matches(['#', '@']);
    let has = |s: &Option<String>| s.as_deref().is_some_and(|s| s.to_lowercase().contains(needle));
    let hits: Vec<&Item> = rows.iter().filter(|r| r.listed).map(|r| &r.item).filter(|i| needle.is_empty() || i.label.to_lowercase().contains(needle) || has(&i.key) || has(&i.hint)).collect();
    let from = q.cursor.as_deref().and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
    let items: Vec<Item> = hits.iter().skip(from).take(q.limit).map(|i| (*i).clone()).collect();
    let next = (from + q.limit < hits.len()).then(|| (from + q.limit).to_string());
    Page { items, next, problem: None }
}

impl Cache {
    fn fresh(&self, k: &Key) -> Option<Page> {
        self.pages.lock().get(k).filter(|(at, _)| at.elapsed() < TTL).map(|(_, p)| p.clone())
    }

    /// The page cached for `k`, however old (what a rate-limited source still shows).
    fn stale(&self, k: &Key) -> Option<Page> {
        self.pages.lock().get(k).map(|(_, p)| p.clone())
    }

    fn put(&self, k: Key, p: Page) {
        let mut pages = self.pages.lock();
        if pages.len() >= MAX_ENTRIES {
            pages.retain(|_, (at, _)| at.elapsed() < TTL);
            if pages.len() >= MAX_ENTRIES
                && let Some(oldest) = pages.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone())
            {
                pages.remove(&oldest);
            }
        }
        pages.insert(k, (Instant::now(), p));
    }

    fn cooling(&self, owner: &str, source: &str) -> Option<u64> {
        let mut c = self.cooling.lock();
        let k = (owner.to_string(), source.to_string());
        match c.get(&k) {
            Some(until) if *until > Instant::now() => Some(until.duration_since(Instant::now()).as_secs().max(1)),
            Some(_) => {
                c.remove(&k);
                None
            }
            None => None,
        }
    }

    fn cool(&self, owner: &str, source: &str, secs: u64) {
        self.cooling.lock().insert((owner.to_string(), source.to_string()), Instant::now() + Duration::from_secs(secs.clamp(1, 3600)));
    }

    /// A whole list of the owner's `source` read within [`TTL`].
    pub(crate) fn list(&self, owner: &str, source: &str, name: &str) -> Option<Rows> {
        let k = (owner.to_string(), source.to_string(), name.to_string());
        self.lists.lock().get(&k).filter(|(at, _)| at.elapsed() < TTL).map(|(_, l)| l.clone())
    }

    pub(crate) fn keep_list(&self, owner: &str, source: &str, name: &str, rows: Rows) {
        let mut lists = self.lists.lock();
        if lists.len() >= MAX_ENTRIES {
            lists.retain(|_, (at, _)| at.elapsed() < TTL);
        }
        if lists.len() < MAX_ENTRIES {
            lists.insert((owner.to_string(), source.to_string(), name.to_string()), (Instant::now(), rows));
        }
    }

    /// Forget the owner's pages of `source` and any rate limit it was cooling off (after a write
    /// through roll: a new label, a connection made or removed).
    pub fn invalidate(&self, owner: &str, source: &str) {
        self.pages.lock().retain(|k, _| !(k.owner == owner && k.source == source));
        self.lists.lock().retain(|(o, s, _), _| !(o == owner && s == source));
        self.cooling.lock().remove(&(owner.to_string(), source.to_string()));
    }
}

impl Code {
    /// Drop the owner's cached lookups of `source` (`linear`, `slack`, `discord`, `github`).
    pub(crate) fn lookups_changed(&self, owner_id: &str, source: &str) {
        self.agents.lookups.invalidate(owner_id, source);
    }
}

// ── the routes ──

/// The sources and kinds there are, for 404s that say what is.
const SOURCES: &[(&str, &[&str])] = &[("linear", linear::KINDS), ("code", rollcode::KINDS), ("slack", slack::KINDS), ("discord", discord::KINDS)];

fn known(source: &str, kind: &str) -> Result<()> {
    let Some((_, kinds)) = SOURCES.iter().find(|(s, _)| *s == source) else {
        return Err(Error::NotFound(format!("lookup source {source:?} (there are {})", SOURCES.iter().map(|(s, _)| *s).collect::<Vec<_>>().join(", "))));
    };
    if !kinds.contains(&kind) {
        return Err(Error::NotFound(format!("{source} lookup {kind:?} (there are {})", kinds.join(", "))));
    }
    Ok(())
}

async fn fetch(code: &Code, owner_id: &str, source: &str, kind: &str, q: &Query) -> std::result::Result<Page, Failure> {
    match source {
        "linear" => linear::list(code, owner_id, kind, q).await,
        "code" => rollcode::list(code, owner_id, kind, q).await,
        "slack" => slack::list(code, owner_id, kind, q).await,
        "discord" => discord::list(code, owner_id, kind, q).await,
        _ => Err(Failure::Refused(Error::NotFound(format!("lookup source {source:?}")))),
    }
}

/// `GET …/lookup/{source}/{kind}`.
async fn list(State(code): S, caller: Caller, Path((owner, source, kind)): Path<(String, String, String)>, Q(raw): Q<RawQuery>) -> Result<Json<Page>> {
    let o = manageable(&code, &caller, &owner)?;
    known(&source, &kind)?;
    let query = Query::parse(raw)?;
    let key = Key { owner: o.id.clone(), source: source.clone(), kind: kind.clone(), query };
    if source == "code" {
        // roll's own rows are a read of control.db: always current, never cached.
        return fetch(&code, &o.id, &source, &kind, &key.query).await.map(Json).map_err(|f| match f {
            Failure::Refused(e) => e,
            Failure::Problem(p) => Error::Unprocessable(problem_text(&p)),
        });
    }
    let cache = &code.agents.lookups;
    if let Some(p) = cache.fresh(&key) {
        return Ok(Json(p));
    }
    if let Some(secs) = cache.cooling(&o.id, &source) {
        let mut p = cache.stale(&key).unwrap_or_default();
        p.problem = Some(Problem::RateLimited { retry_after: secs });
        return Ok(Json(p));
    }
    match fetch(&code, &o.id, &source, &kind, &key.query).await {
        Ok(p) => {
            cache.put(key, p.clone());
            Ok(Json(p))
        }
        Err(Failure::Problem(Problem::RateLimited { retry_after })) => {
            cache.cool(&o.id, &source, retry_after);
            let mut p = cache.stale(&key).unwrap_or_default();
            p.problem = Some(Problem::RateLimited { retry_after });
            Ok(Json(p))
        }
        Err(Failure::Problem(p)) => Ok(Json(Page::problem(p))),
        Err(Failure::Refused(e)) => Err(e),
    }
}

/// `POST …/lookup/{source}/{kind}` `{name, parent?, color?}`: make one (a Linear label) where the
/// platform lets roll, and answer its row. The owner's cached pages of the source go.
async fn create(State(code): S, caller: Caller, Path((owner, source, kind)): Path<(String, String, String)>, Body(b): Body) -> Result<Json<Item>> {
    let o = manageable(&code, &caller, &owner)?;
    known(&source, &kind)?;
    let name = b["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| Error::Invalid("say the new one's name (`name`)".into()))?;
    if name.chars().count() > 80 {
        return Err(Error::Invalid("a name is at most 80 characters".into()));
    }
    let new = New { name: name.to_string(), parent: b["parent"].as_str().map(str::to_string).filter(|p| !p.is_empty()), color: b["color"].as_str().filter(|c| is_color(c)).map(str::to_string) };
    let made = match source.as_str() {
        "linear" => linear::create(&code, &o.id, &kind, &new).await,
        "slack" | "discord" => Err(Failure::Refused(Error::Unprocessable(format!("roll can't make {source} {kind}")))),
        _ => Err(Failure::Refused(Error::NotFound(format!("lookup source {source:?}")))),
    };
    match made {
        Ok(item) => {
            code.lookups_changed(&o.id, &source);
            Ok(Json(item))
        }
        Err(Failure::Problem(p)) => Err(Error::Unprocessable(problem_text(&p))),
        Err(Failure::Refused(e)) => Err(e),
    }
}

/// What a create was asked to make.
pub struct New {
    pub name: String,
    pub parent: Option<String>,
    pub color: Option<String>,
}

fn problem_text(p: &Problem) -> String {
    match p {
        Problem::NotConnected { message } | Problem::Reconnect { message } | Problem::Unavailable { message } => message.clone(),
        Problem::RateLimited { retry_after } => format!("rate limited: try again in {retry_after} s"),
    }
}

/// How a config field asks for a picker (the catalog's convention, docs/code/automations-ui.md):
/// `format: "lookup"` and a `lookup` object saying where its rows come from.
#[derive(Clone, Copy, Default)]
pub struct Pick<'a> {
    /// `source` and `kind` of [`routes`]; `None`: only `extras`.
    pub from: Option<(&'a str, &'a str)>,
    /// The sibling field whose value is the rows' `parent` (a team for its states).
    pub parent: Option<&'a str>,
    /// Rows that aren't the platform's, shown first: `(value, label)` (`roll`, `none`, a type).
    pub extras: &'a [(&'a str, &'a str)],
    /// The heading `extras` show under.
    pub extras_group: Option<&'a str>,
    /// A typed name that matches nothing can be made (`POST`).
    pub create: bool,
}

impl Pick<'_> {
    pub fn from<'a>(source: &'a str, kind: &'a str) -> Pick<'a> {
        Pick { from: Some((source, kind)), ..Pick::default() }
    }

    fn lookup(&self) -> Value {
        let mut l = serde_json::json!({});
        if let Some((source, kind)) = self.from {
            l["source"] = source.into();
            l["kind"] = kind.into();
        }
        if let Some(p) = self.parent {
            l["parent"] = p.into();
        }
        if !self.extras.is_empty() {
            l["extras"] = self.extras.iter().map(|(id, label)| serde_json::json!({ "id": id, "label": label })).collect();
        }
        if let Some(g) = self.extras_group {
            l["extras_group"] = g.into();
        }
        if self.create {
            l["create"] = true.into();
        }
        l
    }

    /// A field holding one value.
    pub fn one(&self, title: &str, description: &str) -> Value {
        serde_json::json!({ "type": "string", "title": title, "description": description, "format": "lookup", "lookup": self.lookup() })
    }

    /// A field holding a list of values.
    pub fn many(&self, title: &str, description: &str) -> Value {
        serde_json::json!({ "type": "array", "items": { "type": "string" }, "title": title, "description": description, "format": "lookup", "lookup": self.lookup() })
    }
}

/// A string at `v[path…]`.
pub(crate) fn s<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().fold(v, |v, k| &v[*k]).as_str().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_pass_on_only_safe_pictures_and_colours() {
        let i = Item::new("u1", "Ada\u{7}").avatar(Some("http://x/a.png")).color(Some("red"));
        assert_eq!((i.label.as_str(), i.avatar, i.color), ("Ada", None, None));
        let i = Item::new("u1", "Ada").avatar(Some("https://x/a.png")).color(Some("#5E6AD2"));
        assert_eq!((i.avatar.as_deref(), i.color.as_deref()), (Some("https://x/a.png"), Some("#5e6ad2")));
        assert_eq!(Item::new("x".repeat(500), "y").id.len(), MAX_TEXT);
    }

    #[test]
    fn queries_are_bounded() {
        let q = Query::parse(RawQuery { q: Some(format!("  {}  ", "a".repeat(300))), parent: None, cursor: None, ids: Some("a, b,,c".into()), limit: Some(10_000) }).unwrap();
        assert_eq!((q.q.len(), q.ids.len(), q.limit), (MAX_Q, 3, MAX_LIMIT));
        let many = (0..=MAX_IDS).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert!(Query::parse(RawQuery { q: None, parent: None, cursor: None, ids: Some(many), limit: None }).is_err());
        assert!(Query::parse(RawQuery { q: None, parent: Some("p".repeat(201)), cursor: None, ids: None, limit: None }).is_err());
    }

    #[test]
    fn the_cache_forgets_an_owners_source_and_cools_off() {
        let c = Cache::default();
        let k = |owner: &str, source: &str| Key { owner: owner.into(), source: source.into(), kind: "teams".into(), query: Query::default() };
        c.put(k("o1", "linear"), Page { items: vec![Item::new("t", "T")], ..Page::default() });
        c.put(k("o2", "linear"), Page::default());
        c.put(k("o1", "slack"), Page::default());
        assert_eq!(c.fresh(&k("o1", "linear")).unwrap().items.len(), 1);
        c.invalidate("o1", "linear");
        assert!(c.fresh(&k("o1", "linear")).is_none());
        assert!(c.fresh(&k("o2", "linear")).is_some() && c.fresh(&k("o1", "slack")).is_some());
        assert_eq!(c.cooling("o1", "linear"), None);
        c.cool("o1", "linear", 30);
        assert!(c.cooling("o1", "linear").is_some_and(|s| s <= 30));
        assert_eq!(c.cooling("o2", "linear"), None);
    }
}
