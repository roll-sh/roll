//! Using a key without seeing it (docs/secrets.md, "Using a key without seeing it"): a key bound
//! to hosts ([`Hosts`]) reaches a consumer that asks for placeholders as `rlph_…`, which roll's
//! egress proxies swap for the value ([`Secrets::substitute`], through [`crate::proxy`]) only in
//! requests to those hosts.
//!
//! A placeholder is random (never derived from the value), stored only as its SHA-256, and bound to
//! (project, environment, folder, key, consumer, who asked, the person who made the consumer,
//! expiry). It stands only for a literal value (never one holding a `${…}` reference), and for a
//! sensitive key only when the binding lets untrusted consumers use it. Every swap checks it all
//! again: nothing here is cached but the audit's once-a-minute counts ([`Uses`]).

use crate::access::{Action, Caller, Scope};
use crate::consumers::ResourceRef;
use crate::links::Mode;
use crate::proxy::{PREFIX, Place, Position, Refusal, Swap};
use crate::{Error, Result, Secrets, Value, envs, json, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS key_hosts (
    secret_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    binding TEXT NOT NULL,
    set_by TEXT NOT NULL,
    set_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS key_hosts_project ON key_hosts (project_id);
CREATE TABLE IF NOT EXISTS placeholders (
    id TEXT PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    key TEXT NOT NULL,
    secret_id TEXT NOT NULL,
    consumer TEXT NOT NULL,
    issued_by TEXT NOT NULL,
    maker TEXT,
    resource TEXT,
    untrusted INTEGER NOT NULL,
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS placeholders_consumer ON placeholders (consumer, revoked_at);
CREATE INDEX IF NOT EXISTS placeholders_project ON placeholders (project_id, issued_at);
CREATE INDEX IF NOT EXISTS placeholders_expiry ON placeholders (expires_at);
CREATE INDEX IF NOT EXISTS placeholders_secret ON placeholders (secret_id, revoked_at);
";

/// How long a placeholder lives unless the resolve says (minutes), and at most.
const DEFAULT_MINUTES: u64 = 24 * 60;
const MAX_MINUTES: u64 = 30 * 24 * 60;
const MINUTE: i64 = 60_000;
const DAY: i64 = 86_400_000;
/// (placeholder, host) pairs counted between audit rows, before all are written at once.
const USES_MAX: usize = 10_000;

/// Where a key's value may go: its hosts, and optionally the one header or query parameter that
/// carries it. Set by an admin (`PUT …/secrets/{key}/hosts`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Hosts {
    /// Hostnames (`api.stripe.com`) or one-label wildcards (`*.stripe.com`).
    pub hosts: Vec<String>,
    /// `Authorization: Bearer {}`: only that header, written exactly so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// `api_key`: only as that query parameter's whole value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// A sensitive key's placeholder also goes to readers that couldn't read its value themselves
    /// (untrusted code: a fork's preview, a sandbox; a person's token). Off: only roll's trusted
    /// runtimes get one for a sensitive key.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untrusted: bool,
}

impl Hosts {
    /// The binding as stored: lower-cased hosts, each checked, and at most one placement, which
    /// `untrusted` needs.
    pub fn checked(mut self) -> Result<Hosts> {
        let bad = Error::BadRequest;
        if self.hosts.is_empty() || self.hosts.len() > 16 {
            return Err(bad("hosts: 1 to 16 of them".into()));
        }
        for h in self.hosts.iter_mut() {
            *h = h.trim().to_ascii_lowercase();
            host_pattern(h).map_err(|why| bad(format!("hosts: {why}")))?;
        }
        self.hosts.sort();
        self.hosts.dedup();
        if self.header.is_some() && self.query.is_some() {
            return Err(bad("a header or a query parameter carries the key, not both".into()));
        }
        if let Some(h) = &self.header {
            template(h).map_err(bad)?;
        }
        if let Some(q) = &self.query
            && (q.is_empty() || q.len() > 64 || !q.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~')))
        {
            return Err(bad("query: a parameter name, 1 to 64 of A-Z, a-z, 0-9, _ - . ~".into()));
        }
        if self.untrusted && !self.placed() {
            return Err(Error::Invalid("placement_required", "untrusted: true needs a placement: a header (\"Authorization: Bearer {}\") or a query parameter, so the code holding the placeholder can't pick a header the host echoes".into()));
        }
        Ok(self)
    }

    /// Whether it names the header or query parameter that carries the key. One without (bound
    /// before a placement was required) lets a trusted reader's placeholder ride in any header.
    pub fn placed(&self) -> bool {
        self.header.is_some() || self.query.is_some()
    }

    /// Whether `host` (as the proxy connects to it, port and final dot allowed) is one of these.
    pub fn allows(&self, host: &str) -> bool {
        normalize_host(host).is_some_and(|h| self.hosts.iter().any(|p| host_matches(p, &h)))
    }

    /// Whether `placeholder` sits where the binding says the key goes.
    fn place(&self, placeholder: &str, at: &Position<'_>) -> std::result::Result<(), String> {
        match (&self.header, &self.query) {
            (Some(t), _) => {
                let (name, before, after) = template(t).map_err(|_| "the key's header template doesn't parse".to_string())?;
                let ok = at.place == Place::Header && at.name.eq_ignore_ascii_case(&name) && at.value.strip_prefix(before.as_str()).and_then(|r| r.strip_suffix(after.as_str())) == Some(placeholder);
                if ok { Ok(()) } else { Err(format!("the key goes only in its header, as {t}")) }
            }
            (None, Some(q)) => {
                if at.place == Place::Query && at.name == q && at.value == placeholder { Ok(()) } else { Err(format!("the key goes only in the query, as {q}")) }
            }
            (None, None) => {
                if at.place == Place::Header { Ok(()) } else { Err("the key goes only in a header".into()) }
            }
        }
    }

    fn describe(&self) -> String {
        let place = match (&self.header, &self.query) {
            (Some(h), _) => format!(" (header {h})"),
            (None, Some(q)) => format!(" (query {q})"),
            _ => String::new(),
        };
        let untrusted = if self.untrusted { ", untrusted consumers too" } else { "" };
        format!("{}{place}{untrusted}", self.hosts.join(", "))
    }

    /// Whether a reader may get a placeholder for (or a swap of) a key with these `tags`: a
    /// sensitive one only for a runtime that could read it, or with the binding's opt-in.
    pub(crate) fn admits(&self, tags: &[String], runtime: bool) -> bool {
        runtime || self.untrusted || !names::sensitive(tags)
    }
}

fn label(l: &str) -> bool {
    !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !l.starts_with('-') && !l.ends_with('-')
}

/// A host a key may be bound to: a name of two labels or more, or `*.` and one (one label).
fn host_pattern(p: &str) -> std::result::Result<(), String> {
    let name = p.strip_prefix("*.").unwrap_or(p);
    let labels: Vec<&str> = name.split('.').collect();
    if p.len() > 253 || labels.len() < 2 || !labels.iter().all(|l| label(l)) {
        return Err(format!("{p} isn't a hostname of two labels or more (a wildcard is only *. and one label, as *.stripe.com)"));
    }
    if labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        return Err(format!("{p} is an address: name a host"));
    }
    if p.starts_with("*.") && public_suffix(name) {
        return Err(format!("{p}: anyone can register a name under {name} (the Public Suffix List says so), so a wildcard there reaches hosts nobody vouched for; bind the exact host"));
    }
    Ok(())
}

/// The Public Suffix List's rules (`assets/public_suffix_list.dat`): names, `*.` rules (every
/// name one label under them) and `!` exceptions. Only ASCII names: a host binding is ASCII.
struct Suffixes {
    exact: std::collections::HashSet<&'static str>,
    wild: std::collections::HashSet<&'static str>,
    except: std::collections::HashSet<&'static str>,
}

static SUFFIXES: std::sync::LazyLock<Suffixes> = std::sync::LazyLock::new(|| {
    let mut s = Suffixes { exact: Default::default(), wild: Default::default(), except: Default::default() };
    for line in include_str!("../assets/public_suffix_list.dat").lines() {
        let Some(rule) = line.split_whitespace().next().filter(|r| !r.starts_with("//") && r.is_ascii()) else { continue };
        if let Some(r) = rule.strip_prefix("*.") {
            s.wild.insert(r);
        } else if let Some(r) = rule.strip_prefix('!') {
            s.except.insert(r);
        } else {
            s.exact.insert(rule);
        }
    }
    s
});

/// Whether names one label under `name` are anyone's to register (a host binding's wildcard here,
/// and an interception CA's: `intercept.rs`).
pub(crate) fn public_suffix(name: &str) -> bool {
    let s = &*SUFFIXES;
    !s.except.contains(name) && (s.exact.contains(name) || name.split_once('.').is_some_and(|(_, parent)| s.wild.contains(parent)))
}

/// The request's host as patterns are matched: lower case, no port, no final dot. `None` for an
/// IPv6 literal or anything that isn't a name.
fn normalize_host(h: &str) -> Option<String> {
    let mut h = h.trim().to_ascii_lowercase();
    if let Some((name, port)) = h.rsplit_once(':')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && !name.contains(':')
    {
        h = name.to_string();
    }
    let h = h.strip_suffix('.').unwrap_or(&h).to_string();
    (h.split('.').count() >= 2 && h.split('.').all(label)).then_some(h)
}

fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(rest) => host.split_once('.').is_some_and(|(first, r)| !first.is_empty() && r == rest),
        None => pattern == host,
    }
}

/// `Name: before{}after` as (name, before, after).
fn template(t: &str) -> std::result::Result<(String, String, String), String> {
    let bad = || format!("header: a template such as \"Authorization: Bearer {{}}\" or \"X-Api-Key: {{}}\", not {t:?}");
    let (name, value) = t.split_once(':').ok_or_else(bad)?;
    let name = name.trim();
    let value = value.trim_start();
    let tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if t.len() > 256 || name.is_empty() || !name.bytes().all(tchar) || value.matches("{}").count() != 1 || value.bytes().any(|b| b.is_ascii_control()) {
        return Err(bad());
    }
    if ["host", "content-length", "transfer-encoding", "connection"].contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("header: {name} can't carry a key"));
    }
    let (before, after) = value.split_once("{}").ok_or_else(bad)?;
    Ok((name.to_string(), before.to_string(), after.to_string()))
}

/// A placeholder a resolve answered (`placeholders` in `Values`): never the placeholder itself.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Placeholder {
    pub key: String,
    /// Its record (`phd_…`).
    pub id: String,
    pub hosts: Vec<String>,
    pub expires_at: i64,
}

/// A placeholder's record (`GET …/placeholders`).
#[derive(Clone, Debug, Serialize)]
pub struct PlaceholderRecord {
    pub id: String,
    pub environment: String,
    pub path: String,
    pub key: String,
    pub consumer: String,
    pub issued_by: String,
    /// Who made the consumer, for an in-process one.
    pub maker: Option<String>,
    pub issued_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

/// What a resolve asking for placeholders says (`Secrets::values_with_placeholders`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ask<'a> {
    /// How long they live (default a day, at most 30 days).
    pub minutes: Option<u64>,
    /// Who made the consumer (a person `usr_…`, a token `token:…`, or `operator`): required for an
    /// in-process consumer; its placeholders swap only while they may reveal there.
    pub maker: Option<&'a str>,
}

/// Who a resolve issues placeholders for.
pub(crate) struct Issue<'a> {
    pub consumer: &'a str,
    pub issued_by: &'a str,
    /// The person (or token, or the operator) who made the consumer: an in-process consumer's
    /// placeholders swap only while they may still reveal there.
    pub maker: Option<&'a str>,
    pub resource: Option<&'a ResourceRef>,
    pub untrusted: bool,
    pub minutes: Option<u64>,
}

/// Whether a stored value holds a `${…}` reference: its `refs` column, or the value itself for a
/// row from before references were kept.
pub(crate) fn holds_reference(dek: &crate::crypto::Dek, project: &str, id: &str, version: i64, refs: Option<&[String]>, value: Option<&[u8]>) -> bool {
    match (refs, value) {
        (Some(r), _) => !r.is_empty(),
        // One that doesn't open counts as holding one: nothing to vouch for.
        (None, Some(ct)) => match dek.open(&crate::crypto::value_aad(project, id, version), ct) {
            Ok(v) => !crate::refs::refs(v.expose()).is_empty(),
            Err(_) => true,
        },
        (None, None) => false,
    }
}

fn hash(token: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A host as an error or an audit row may say it: printable and short.
fn shown(host: &str) -> String {
    host.chars().filter(|c| !c.is_control()).take(255).collect()
}

/// Swaps counted since each (placeholder, host, refused) pair's last audit row, so a busy one
/// writes a row a minute.
#[derive(Default)]
pub(crate) struct Uses(std::sync::Mutex<HashMap<(String, String, bool), Use>>);

#[derive(Clone)]
struct Use {
    project: String,
    env: String,
    path: String,
    key: String,
    consumer: String,
    pending: u64,
    logged_at: i64,
    /// A refusal's hosts since its last row (at most [`HOST_SAMPLE`]), and whether there were more.
    hosts: Vec<String>,
    others: bool,
}

/// The hosts a wrong-host alert names: the rest are counted.
const HOST_SAMPLE: usize = 5;

/// A placeholder's row, as a swap checks it.
struct Row {
    id: String,
    project: String,
    env_id: String,
    path: String,
    key: String,
    consumer: String,
    issued_by: String,
    maker: Option<String>,
    resource: Option<String>,
    untrusted: bool,
    expires_at: i64,
    revoked_at: Option<i64>,
}

impl Secrets {
    /// The project's bindings, by secret id.
    pub(crate) fn bindings(&self, project: &str) -> Result<HashMap<String, Hosts>> {
        let rows = self.db.read(|c| c.query("SELECT secret_id, binding FROM key_hosts WHERE project_id = ?", [project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
        Ok(rows.into_iter().filter_map(|(id, b)| serde_json::from_str(&b).ok().map(|b| (id, b))).collect())
    }

    pub(crate) fn binding(&self, secret_id: &str) -> Result<Option<Hosts>> {
        let b: Option<String> = self.db.read(|c| c.query_row("SELECT binding FROM key_hosts WHERE secret_id = ?", [secret_id], |r| r.get(0)).optional())?;
        Ok(b.and_then(|b| serde_json::from_str(&b).ok()))
    }

    /// Bind shared key `key` of `slug`'s own folder `path` to `hosts`: an admin's, audited.
    pub fn set_hosts(&self, project: &str, slug: &str, path: &str, key: &str, hosts: Hosts, caller: &Caller) -> Result<Hosts> {
        let path = names::path(path)?;
        caller.need(Action::Admin, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let hosts = hosts.checked()?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            type Found = (String, Option<String>, Option<String>, Option<Vec<u8>>, i64);
            let row: Option<Found> = c
                .query_row("SELECT id, link, refs, value, version FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ''", params![env.id, path, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .optional()?;
            let Some((id, link, refs, value, version)) = row else {
                return Err(Error::NotFound(format!("no shared secret {key} in {slug}:{path} (hosts bind a key of the environment's own folder)")));
            };
            if link.as_deref().and_then(|l| serde_json::from_str::<crate::Link>(l).ok()).is_some_and(|l| l.mode == Mode::Dynamic) {
                return Err(Error::BadRequest(format!("{key} is a dynamic credential: a placeholder can't stand for a lease issued per read")));
            }
            let refs: Option<Vec<String>> = refs.as_deref().map(crate::from_json);
            if holds_reference(&dek, project, &id, version, refs.as_deref(), value.as_deref()) {
                return Err(Error::BadRequest(format!("{key} holds a ${{…}} reference: a placeholder stands only for a literal value (a reference would send another key's value to these hosts)")));
            }
            c.execute(
                "INSERT INTO key_hosts (secret_id, project_id, binding, set_by, set_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (secret_id) DO UPDATE SET binding = excluded.binding, set_by = excluded.set_by, set_at = excluded.set_at",
                params![id, project, json(&hosts), caller.principal, now],
            )?;
            Ok::<_, Error>(())
        })?;
        self.audit_now(project, caller, "secrets.hosts", Some(slug), Some(&path), &[key.to_string()], &format!("bound {key} to {}", hosts.describe()));
        Ok(hosts)
    }

    /// Take `key`'s binding off: its placeholders stop working at their next use.
    pub fn clear_hosts(&self, project: &str, slug: &str, path: &str, key: &str, caller: &Caller) -> Result<()> {
        let path = names::path(path)?;
        caller.need(Action::Admin, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        let gone = self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            Ok::<_, Error>(c.execute("DELETE FROM key_hosts WHERE secret_id IN (SELECT id FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = '')", params![env.id, path, key])?)
        })?;
        if gone == 0 {
            return Err(Error::NotFound(format!("{key} in {slug}:{path} isn't bound to hosts")));
        }
        self.audit_now(project, caller, "secrets.hosts", Some(slug), Some(&path), &[key.to_string()], &format!("unbound {key} from its hosts"));
        Ok(())
    }

    /// Placeholders for `keys` (key and binding) of `env_id`'s folder `path`, in one write: each
    /// returned beside its record.
    /// `keys` are (key, its secret's id, its binding).
    pub(crate) fn issue_placeholders(&self, project: &str, env_id: &str, path: &str, keys: &[(String, String, Hosts)], how: &Issue<'_>) -> Result<Vec<(Value, Placeholder)>> {
        if keys.is_empty() {
            return Ok(vec![]);
        }
        let minutes = how.minutes.unwrap_or(DEFAULT_MINUTES);
        if minutes == 0 || minutes > MAX_MINUTES {
            return Err(Error::BadRequest(format!("placeholder_minutes: 1 to {MAX_MINUTES}")));
        }
        let now = self.db.now_ms()?;
        let expires_at = now + minutes as i64 * MINUTE;
        let resource = how.resource.map(|r| serde_json::json!({ "kind": r.kind.as_str(), "ref": r.r#ref }).to_string());
        let out: Vec<(Value, Placeholder, &str)> = keys
            .iter()
            .map(|(key, secret, b)| (Value::new(roll_auth::credentials::new_secret(PREFIX)), Placeholder { key: key.clone(), id: names::new_id("phd", 16), hosts: b.hosts.clone(), expires_at }, secret.as_str()))
            .collect();
        self.db.write(|c| {
            for (token, p, secret) in &out {
                c.execute(
                    "INSERT INTO placeholders (id, hash, project_id, env_id, path, key, secret_id, consumer, issued_by, maker, resource, untrusted, issued_at, expires_at, revoked_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
                    params![p.id, hash(token.expose()), project, env_id, path, p.key, secret, how.consumer, how.issued_by, how.maker, resource, how.untrusted as i64, now, expires_at],
                )?;
            }
            Ok::<_, Error>(())
        })?;
        Ok(out.into_iter().map(|(t, p, _)| (t, p)).collect())
    }

    fn placeholder_row(&self, token: &str) -> Result<Option<Row>> {
        let h = hash(token);
        Ok(self.db.read(|c| {
            c.query_row(
                "SELECT id, project_id, env_id, path, key, consumer, issued_by, maker, resource, untrusted, expires_at, revoked_at FROM placeholders WHERE hash = ?",
                [&h],
                |r| Ok(Row { id: r.get(0)?, project: r.get(1)?, env_id: r.get(2)?, path: r.get(3)?, key: r.get(4)?, consumer: r.get(5)?, issued_by: r.get(6)?, maker: r.get(7)?, resource: r.get(8)?, untrusted: r.get::<_, i64>(9)? != 0, expires_at: r.get(10)?, revoked_at: r.get(11)? }),
            )
            .optional()
        })?)
    }

    /// `by` as the caller it is now: the operator, a roll-access token as roll-access answers now,
    /// a person who can still sign in. `None` for an in-process consumer's name (`sandbox:…`).
    fn principal_caller(&self, by: &str) -> std::result::Result<Option<Caller>, Refusal> {
        let no = |m: &str| Refusal::NotAllowed(m.to_string());
        let failed = |e: Error| Refusal::Failed(e.to_string());
        if by == "operator" {
            return Ok(Some(Caller::operator()));
        }
        if let Some(id) = by.strip_prefix("token:") {
            let g = self.grants.as_ref().and_then(|g| g.grant(id)).ok_or_else(|| no("the token behind it is gone"))?;
            return Ok(Some(Caller::token(id, self.grant_grants(&g).map_err(failed)?)));
        }
        if crate::access::is_person(by) {
            let p = self.person(by).map_err(failed)?.ok_or_else(|| no("the person behind it can't sign in any more"))?;
            return self.person_caller(by, p.admin).map(Some).map_err(failed);
        }
        if by.starts_with("identity:") {
            return Err(no("a workload identity's login doesn't take placeholders"));
        }
        Ok(None)
    }

    /// Whether `maker` (who made an in-process consumer) still reaches `project` and may reveal
    /// `slug:path`, read as the consumer is (`untrusted`): asked again on every use.
    pub(crate) fn maker_may(&self, maker: &str, project: &str, slug: &str, path: &str, untrusted: bool) -> std::result::Result<(), Refusal> {
        let failed = |e: Error| Refusal::Failed(e.to_string());
        let gone = || Refusal::NotAllowed(format!("{maker}, who made the consumer, may no longer reveal {slug}:{path}"));
        if !self.maker_reaches(maker, project).map_err(failed)? {
            return Err(gone());
        }
        let c = self.principal_caller(maker)?.ok_or_else(|| Refusal::NotAllowed(format!("{maker} isn't a person, a token or the operator")))?;
        let c = if untrusted { self.untrusted(project, &c).map_err(failed)?.not_runtime() } else { c };
        let c = self.caller_for(project, slug, &c).map_err(failed)?;
        if c.allows(Action::Reveal, &Scope::at(project, slug, path)) { Ok(()) } else { Err(gone()) }
    }

    /// Who reads for a placeholder now: the resource it was issued for, if its project still
    /// stands for it, or whoever asked, as roll-access and the accounts answer now.
    fn placeholder_reader(&self, row: &Row) -> std::result::Result<Caller, Refusal> {
        let no = |m: &str| Refusal::NotAllowed(m.to_string());
        if let Some(r) = &row.resource {
            let r: serde_json::Value = serde_json::from_str(r).map_err(|_| Refusal::Failed("a placeholder's resource isn't JSON".into()))?;
            let kind = roll_workspace::Kind::parse(r["kind"].as_str().unwrap_or_default()).map_err(|_| no("its resource's kind is gone"))?;
            let mut resource = ResourceRef::new(kind, r["ref"].as_str().unwrap_or_default());
            if row.untrusted {
                resource = resource.untrusted();
            }
            return match self.project_for(&resource, Some(&row.project)) {
                Ok(p) if p == row.project => Ok(Caller::consumer(&resource.consumer())),
                _ => Err(no("the resource it was issued for isn't one its project stands for any more")),
            };
        }
        // An in-process consumer (a preview, a sandbox) reads as roll runs it; its maker is
        // checked beside it.
        Ok(self.principal_caller(&row.issued_by)?.unwrap_or_else(|| Caller::consumer(&row.issued_by)))
    }

    /// `placeholder`'s value for a request to `host`, with the placeholder `at` there: refused
    /// unless it's live, whoever asked for it (and whoever made its consumer) still may, and the
    /// key is still bound to `host`, goes `at` there, and holds a literal value. A wrong host is
    /// an alert. Audited once a minute per (placeholder, host). Blocking: in process only, for
    /// roll's egress proxies (`proxy::filter`).
    pub fn substitute(&self, placeholder: &str, host: &str, at: Position<'_>) -> std::result::Result<Swap, Refusal> {
        let failed = |e: Error| Refusal::Failed(e.to_string());
        if !placeholder.starts_with(PREFIX) || !roll_auth::credentials::checksum_ok(placeholder) {
            return Err(Refusal::Unknown);
        }
        let row = self.placeholder_row(placeholder).map_err(failed)?.ok_or(Refusal::Unknown)?;
        let now = self.db.now_ms().map_err(|e| failed(e.into()))?;
        if row.revoked_at.is_some() {
            return Err(Refusal::Revoked);
        }
        if row.expires_at <= now {
            return Err(Refusal::Expired);
        }
        let slug: Option<String> = self.db.read(|c| c.query_row("SELECT slug FROM environments WHERE id = ? AND project_id = ?", [&row.env_id, &row.project], |r| r.get(0)).optional()).map_err(|e| failed(e.into()))?;
        let slug = slug.ok_or_else(|| Refusal::NotAllowed("its environment is gone".into()))?;
        let (project, path, key) = (row.project.as_str(), row.path.as_str(), row.key.as_str());
        let reader = self.placeholder_reader(&row)?;
        let reader = if row.untrusted { self.untrusted(project, &reader).map_err(failed)?.not_runtime() } else { reader };
        let mut reader = self.caller_for(project, &slug, &reader).map_err(failed)?;
        if !reader.allows(Action::Reveal, &Scope::at(project, &slug, path)) {
            return Err(Refusal::NotAllowed(format!("{slug} is protected, or whoever asked for the placeholder may no longer read it")));
        }
        if let Some(m) = &row.maker {
            self.maker_may(m, project, &slug, path, row.untrusted)?;
        }
        let e = self.entry_for(project, &slug, path, key, &reader)?;
        let hosts = self.binding(&e.row.id).map_err(failed)?.ok_or_else(|| Refusal::Unusable(format!("{key} isn't bound to hosts any more")))?;
        if !hosts.allows(host) {
            self.note_use(&row, &slug, host, true, now);
            return Err(Refusal::WrongHost(shown(host)));
        }
        if row.untrusted && !hosts.placed() {
            return Err(Refusal::PlacementRequired);
        }
        hosts.place(placeholder, &at).map_err(Refusal::WrongPlace)?;
        if !hosts.admits(&e.row.tags, reader.runtime) {
            return Err(Refusal::NotAllowed(format!("{key} is sensitive, and its binding doesn't let untrusted consumers use it")));
        }
        if e.row.link.as_ref().is_some_and(|l| l.mode == Mode::Dynamic) {
            return Err(Refusal::Unusable(format!("{key} is a dynamic credential now")));
        }
        let dek = self.project(project).map_err(failed)?;
        if holds_reference(&dek, &e.row.project_id, &e.row.id, e.row.version, e.row.refs.as_deref(), e.row.value.as_deref()) {
            return Err(Refusal::Unusable(format!("{key} holds a reference now: a placeholder stands only for a literal value")));
        }
        // The key's own value as stored (or its link's), never resolved: roll's proxy is the
        // runtime that uses it, so a sensitive key's value reaches it, and only it.
        reader.runtime = true;
        let mut r = crate::read::Resolver::new(self, project, &slug, &dek, &reader, false, &row.consumer).map_err(failed)?;
        r.no_dynamic = true;
        let (value, problem) = r.entry(&e);
        r.finish(&slug).map_err(failed)?;
        if let Some(p) = problem {
            return Err(Refusal::Unusable(format!("{key} can't be read: {p}")));
        }
        self.note_use(&row, &slug, host, false, now);
        Ok(Swap { value, key: row.key, placeholder: row.id })
    }

    /// `host` echoed back the value placeholder `id` (`phd_…`) stood for, in the response to a
    /// request it was swapped into (`proxy::Scrub`'s [`Echoed`](crate::proxy::Echoed)): the
    /// placeholder ends (a host that echoes once will again), with a `secrets.alert` row and the
    /// environment's notify URL. In process only, for roll's egress proxies.
    pub fn echoed(&self, id: &str, host: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        let host = normalize_host(host).unwrap_or_else(|| shown(host));
        let mac = self.keys()?;
        self.db.write(|c| {
            let row: Option<(String, String, String, String, String)> = c
                .query_row("SELECT p.project_id, COALESCE(e.slug, ''), p.path, p.key, p.consumer FROM placeholders p LEFT JOIN environments e ON e.id = p.env_id WHERE p.id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .optional()?;
            let Some((project, env, path, key, consumer)) = row else { return Err(Error::NotFound(format!("no placeholder {id}"))) };
            c.execute("UPDATE placeholders SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL", params![now, id])?;
            let text = format!("{host} echoed {key} back in a response to {consumer}: placeholder {id} is ended, and the response was stopped. Bind {key} to a header or query parameter {host} doesn't echo");
            let alert = serde_json::json!({ "kind": "placeholder_echo", "placeholder": id, "consumer": consumer, "host": host });
            crate::audit::record(c, mac, &project, &Caller::consumer("roll:placeholders"), "secrets.alert", Some(&env), Some(&path), std::slice::from_ref(&key), &text, now)?;
            self.queue_alert(c, &project, &env, &text, alert, now, now)?;
            Ok::<_, Error>(())
        })
    }

    /// `key`'s effective entry in `slug:path` for `reader`, or why not.
    fn entry_for(&self, project: &str, slug: &str, path: &str, key: &str, reader: &Caller) -> std::result::Result<crate::read::Entry, Refusal> {
        match self.view_key(project, slug, path, key, reader) {
            Ok(Some(e)) => Ok(e),
            Ok(None) => Err(Refusal::Unusable(format!("{key} isn't set in {slug}:{path} any more"))),
            Err(Error::Frozen(m)) | Err(Error::NotFound(m)) => Err(Refusal::NotAllowed(m)),
            Err(e) => Err(Refusal::Failed(e.to_string())),
        }
    }

    /// Count a swap (or a refusal for a wrong host), writing its row when the pair's last is a
    /// minute old.
    fn note_use(&self, row: &Row, env: &str, host: &str, refused: bool, now: i64) {
        let host = normalize_host(host).unwrap_or_else(|| shown(host));
        // Swaps per (placeholder, host); refusals per placeholder, whatever the host, so a loop
        // over random hosts is one row a minute, with a count and a few of them.
        let k = (row.id.clone(), if refused { String::new() } else { host.clone() }, refused);
        let mut due = vec![];
        {
            let mut uses = self.uses.0.lock().unwrap_or_else(|e| e.into_inner());
            if uses.len() >= USES_MAX && !uses.contains_key(&k) {
                for (k, u) in uses.iter_mut().filter(|(_, u)| u.pending > 0) {
                    due.push((k.clone(), u.clone()));
                    u.pending = 0;
                    u.hosts.clear();
                    u.others = false;
                }
                uses.clear();
            }
            let u = uses.entry(k.clone()).or_insert_with(|| Use { project: row.project.clone(), env: env.to_string(), path: row.path.clone(), key: row.key.clone(), consumer: row.consumer.clone(), pending: 0, logged_at: i64::MIN, hosts: vec![], others: false });
            u.pending += 1;
            if refused && !u.hosts.contains(&host) {
                if u.hosts.len() < HOST_SAMPLE {
                    u.hosts.push(host);
                } else {
                    u.others = true;
                }
            }
            if now.saturating_sub(u.logged_at) >= MINUTE {
                due.push((k, u.clone()));
                u.pending = 0;
                u.logged_at = now;
                u.hosts.clear();
                u.others = false;
            }
        }
        for (k, u) in due {
            self.write_use(&k, &u, now);
        }
    }

    fn write_use(&self, (id, host, refused): &(String, String, bool), u: &Use, now: i64) {
        let times = if u.pending == 1 { "once".to_string() } else { format!("{} times", u.pending) };
        if !refused {
            let summary = format!("substituted {} for {host} {times} for {} ({id})", u.key, u.consumer);
            self.audit_now(&u.project, &Caller::consumer(&u.consumer), "secrets.substitute", Some(&u.env), Some(&u.path), std::slice::from_ref(&u.key), &summary);
            return;
        }
        let hosts = u.hosts.join(", ");
        tracing::warn!(project = %u.project, env = %u.env, key = %u.key, consumer = %u.consumer, placeholder = %id, %hosts, count = u.pending, "a placeholder was sent to hosts its key isn't bound to");
        let text = format!("placeholder {id} of {}, issued to {}, was sent to hosts {} isn't bound to {times} ({hosts}{}): likely an exfiltration attempt", u.key, u.consumer, u.key, if u.others { " and others" } else { "" });
        let alert = serde_json::json!({ "kind": "placeholder_host", "placeholder": id, "consumer": u.consumer, "hosts": u.hosts, "more_hosts": u.others, "count": u.pending });
        let res = self.keys().and_then(|mac| {
            self.db.write(|c| {
                crate::audit::record(c, mac, &u.project, &Caller::consumer("roll:placeholders"), "secrets.alert", Some(&u.env), Some(&u.path), std::slice::from_ref(&u.key), &text, now)?;
                self.queue_alert(c, &u.project, &u.env, &text, alert.clone(), now, now)?;
                Ok::<_, Error>(())
            })
        });
        if let Err(e) = res {
            tracing::error!(project = %u.project, "recording a placeholder's alert failed: {e}");
        }
    }

    /// Write the counts a quiet pair left (the sweeper), and forget pairs idle for a minute.
    pub(crate) fn flush_uses(&self) -> Result<()> {
        let now = self.db.now_ms()?;
        let due: Vec<_> = {
            let mut uses = self.uses.0.lock().unwrap_or_else(|e| e.into_inner());
            let mut due = vec![];
            for (k, u) in uses.iter_mut() {
                if u.pending > 0 && now.saturating_sub(u.logged_at) >= MINUTE {
                    due.push((k.clone(), u.clone()));
                    u.pending = 0;
                    u.logged_at = now;
                    u.hosts.clear();
                    u.others = false;
                }
            }
            uses.retain(|_, u| u.pending > 0 || now.saturating_sub(u.logged_at) < MINUTE);
            due
        };
        for (k, u) in due {
            self.write_use(&k, &u, now);
        }
        Ok(())
    }

    /// End every live placeholder issued to `consumer` (a sandbox stopped, a preview torn down),
    /// and its interception CAs ([`Secrets::end_intercept_cas`]): the proxy drops them at its next
    /// check. Answers how many placeholders ended.
    pub fn end_consumer(&self, consumer: &str) -> Result<usize> {
        let ended = self.end_placeholders("consumer = ?", consumer, None)?;
        self.end_intercept_cas(consumer, "its consumer ended")?;
        Ok(ended)
    }

    /// End the placeholders matching `cond` (one `?`, `arg`) and, with `caller`, only those of its
    /// project it may delete; each project's audited.
    fn end_placeholders(&self, cond: &str, arg: &str, caller: Option<(&str, &Caller)>) -> Result<usize> {
        let now = self.db.now_ms()?;
        let ended: Vec<(String, String)> = self.db.write(|c| {
            let rows: Vec<(String, String, String, String, String)> = c.query(
                &format!("SELECT p.id, p.project_id, COALESCE(e.slug, ''), p.path, p.consumer FROM placeholders p LEFT JOIN environments e ON e.id = p.env_id WHERE p.revoked_at IS NULL AND p.{cond}"),
                [arg],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            let mut out = vec![];
            for (id, project, env, path, consumer) in rows {
                if let Some((p, who)) = caller
                    && (project != p || !who.allows(Action::Delete, &Scope::at(&project, &env, &path)))
                {
                    continue;
                }
                c.execute("UPDATE placeholders SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL", params![now, id])?;
                out.push((project, consumer));
            }
            Ok::<_, Error>(out)
        })?;
        let mut by_project: std::collections::BTreeMap<(String, String), usize> = Default::default();
        for key in &ended {
            *by_project.entry(key.clone()).or_default() += 1;
        }
        let actor = caller.map_or_else(|| Caller::consumer("roll:placeholders"), |(_, c)| c.clone());
        for ((project, consumer), n) in by_project {
            self.audit_now(&project, &actor, "secrets.placeholders.end", None, None, &[], &format!("ended {} of {consumer}", crate::write::plural(n, "placeholder")));
        }
        Ok(ended.len())
    }

    /// The project's placeholders the caller may read, newest first (never the placeholder).
    pub fn placeholders(&self, project: &str, caller: &Caller) -> Result<Vec<PlaceholderRecord>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let rows = self.db.read(|c| {
            c.query(
                "SELECT p.id, e.slug, p.path, p.key, p.consumer, p.issued_by, p.maker, p.issued_at, p.expires_at, p.revoked_at FROM placeholders p JOIN environments e ON e.id = p.env_id WHERE p.project_id = ? ORDER BY p.issued_at DESC, p.id LIMIT 1000",
                [project],
                |r| Ok(PlaceholderRecord { id: r.get(0)?, environment: r.get(1)?, path: r.get(2)?, key: r.get(3)?, consumer: r.get(4)?, issued_by: r.get(5)?, maker: r.get(6)?, issued_at: r.get(7)?, expires_at: r.get(8)?, revoked_at: r.get(9)? }),
            )
        })?;
        Ok(rows.into_iter().filter(|p| caller.allows(Action::Read, &Scope::at(project, &p.environment, &p.path))).collect())
    }

    /// End placeholder `id` now: whoever may delete in its folder.
    pub fn revoke_placeholder(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        let p = self.placeholders(project, caller)?.into_iter().find(|p| p.id == id).ok_or_else(|| Error::NotFound(format!("no placeholder {id}")))?;
        caller.need(Action::Delete, &Scope::at(project, &p.environment, &p.path))?;
        if p.revoked_at.is_none() && self.end_placeholders("id = ?", id, Some((project, caller)))? > 0 {
            Ok(())
        } else {
            Err(Error::Conflict("conflict", format!("{id} is already revoked")))
        }
    }

    /// End the project's live placeholders of `consumer` that the caller may delete.
    pub fn end_consumer_in(&self, project: &str, consumer: &str, caller: &Caller) -> Result<usize> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.end_placeholders("consumer = ?", consumer, Some((project, caller)))
    }

    /// A leaked placeholder (found in a pushed file, by GitHub's secret scanning): revoked, and
    /// what it was. `None`: it wasn't live.
    pub fn revoke_leaked_placeholder(&self, token: &str, why: &str) -> Option<String> {
        let row = self.placeholder_row(token).ok().flatten().filter(|r| r.revoked_at.is_none())?;
        let now = self.db.now_ms().ok()?;
        let n = self.db.write(|c| c.execute("UPDATE placeholders SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL", params![now, row.id])).ok()?;
        if n == 0 {
            return None;
        }
        let why: String = why.chars().filter(|c| !c.is_control()).take(200).collect();
        self.audit_now(&row.project, &Caller::consumer("roll:leaks"), "secrets.placeholders.end", None, Some(&row.path), std::slice::from_ref(&row.key), &format!("revoked leaked placeholder {} of {} for {}: {why}", row.id, row.key, row.consumer));
        Some(format!("roll secrets placeholder {} of {}", row.id, row.key))
    }

    /// The sweeper's part: quiet pairs' counts, and placeholders a day past their end.
    pub(crate) fn sweep_placeholders(&self) -> Result<()> {
        self.flush_uses()?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("DELETE FROM placeholders WHERE expires_at < ? OR revoked_at < ?", params![now - DAY, now - DAY])?;
            c.execute("DELETE FROM key_hosts WHERE NOT EXISTS (SELECT 1 FROM secrets s WHERE s.id = key_hosts.secret_id)", ())?;
            Ok::<_, Error>(())
        })
    }
}

/// Interception CAs' hosts ([`crate::intercept`]): by project, the hosts of the keys `consumer`
/// holds live placeholders for, as bound now.
impl crate::intercept::BoundHosts for Secrets {
    fn bound_hosts(&self, consumer: &str) -> Result<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>> {
        let now = self.db.now_ms()?;
        let rows: Vec<(String, String)> = self.db.read(|c| {
            c.query(
                "SELECT DISTINCT p.project_id, h.binding FROM placeholders p JOIN key_hosts h ON h.secret_id = p.secret_id WHERE p.consumer = ? AND p.revoked_at IS NULL AND p.expires_at > ?",
                params![consumer, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })?;
        let mut out: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
        for (project, binding) in rows {
            let b: Hosts = serde_json::from_str(&binding).map_err(|e| Error::Internal(format!("a key's hosts: {e}")))?;
            out.entry(project).or_default().extend(b.hosts);
        }
        Ok(out)
    }
}

/// roll's egress proxies swap through this, in process ([`crate::proxy::filter`]).
impl crate::proxy::Substitute for Secrets {
    fn substitute(&self, placeholder: &str, host: &str, at: Position<'_>) -> std::result::Result<Swap, Refusal> {
        Secrets::substitute(self, placeholder, host, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_names_or_one_label_wildcards() {
        let h = |hosts: &[&str]| Hosts { hosts: hosts.iter().map(|s| s.to_string()).collect(), header: None, query: None, untrusted: false }.checked();
        let b = h(&["API.Stripe.com", "*.stripe.com"]).unwrap();
        assert!(b.allows("api.stripe.com") && b.allows("files.stripe.com:443") && b.allows("API.STRIPE.COM."));
        assert!(!b.allows("stripe.com") && !b.allows("a.b.stripe.com") && !b.allows("api.stripe.com.evil.example") && !b.allows("[::1]:443"));
        for bad in [&["*"][..], &["*.com"], &["com"], &["10.0.0.1"], &["api.stripe.com:443"], &["*.*.stripe.com"], &["a_b.example.com"], &[]] {
            assert!(h(bad).is_err(), "{bad:?}");
        }
        // Not a wildcard anyone can register under (the Public Suffix List's private and ICANN
        // names); an exact host there, or a wildcard under someone's own name there, is fine.
        for shared in ["*.supabase.co", "*.vercel.app", "*.workers.dev", "*.github.io", "*.co.uk", "*.s3.amazonaws.com"] {
            assert!(h(&[shared]).is_err(), "{shared}");
        }
        assert!(h(&["myproject.supabase.co", "*.shop.vercel.app", "*.example.co.uk"]).is_ok());
        let both = Hosts { hosts: vec!["api.stripe.com".into()], header: Some("X-Api-Key: {}".into()), query: Some("key".into()), untrusted: false };
        assert!(both.checked().is_err());
        for bad in ["Authorization Bearer {}", "Host: {}", "X-Key: {} {}", "X-Key: none", "Bad Name: {}"] {
            assert!(Hosts { hosts: vec!["api.stripe.com".into()], header: Some(bad.into()), query: None, untrusted: false }.checked().is_err(), "{bad}");
        }
    }
}
