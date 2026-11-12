//! Changes (docs/secrets.md, "Changes and versions"): every write is one atomic batch on one
//! environment that makes a version per key, numbered by the project's `seq`.

use crate::access::{Action, Caller, Scope};
use crate::crypto::{Dek, value_aad};
use crate::envs::{self, EnvRow};
use crate::links::{Link, Mode};
use crate::{Error, Result, Secrets, Value, audit, from_json, json, names};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// One operation of a batch, as the API takes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub enum Op {
    Set {
        key: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        comment: Option<String>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default, rename = "type")]
        kind: Option<SecretType>,
        /// `null` removes the link.
        #[serde(default, deserialize_with = "some")]
        link: Option<Option<Link>>,
        /// Another folder for this one operation (promote, rollback; default the batch's).
        #[serde(default)]
        path: Option<String>,
    },
    Delete {
        key: String,
        #[serde(default, rename = "type")]
        kind: Option<SecretType>,
        #[serde(default)]
        path: Option<String>,
    },
}

fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecretType {
    #[default]
    Shared,
    Personal,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    pub ops: Vec<Op>,
    /// Fail with `stale` if the environment changed after this change.
    #[serde(default)]
    pub if_seq: Option<i64>,
}

/// One key's operation, resolved to its folder and owner.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub path: String,
    pub key: String,
    pub owner: String,
    pub action: ItemAction,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ItemAction {
    Set { value: Option<Value>, comment: Option<String>, tags: Option<Vec<String>>, link: Option<Option<Link>>, link_state: Option<Json> },
    Delete,
}

impl ItemAction {
    pub fn set_value(v: Value) -> ItemAction {
        ItemAction::Set { value: Some(v), comment: None, tags: None, link: None, link_state: None }
    }
}

/// A change as the API shows it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Change {
    pub seq: i64,
    pub environment: String,
    pub path: String,
    pub kind: String,
    pub message: String,
    pub by: String,
    pub at: i64,
    pub keys: i64,
}

/// What a change did to one key; fingerprints, never values.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ItemDiff {
    pub key: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: SecretType,
    pub what: Vec<String>,
    pub from_fp: Option<String>,
    pub to_fp: Option<String>,
}

/// A managed credential's parts and its link state.
type Credential = (BTreeMap<String, Value>, Json);

/// A credential issued while preparing a write, revoked again if the write fails.
pub(crate) struct Fresh {
    kind: String,
    r#ref: String,
    handle: Json,
}

pub(crate) struct Prepared {
    pub items: Vec<Item>,
    pub fresh: Vec<Fresh>,
}

impl Prepared {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The batch's operations as items, checked (keys, values, comments, tags).
pub(crate) fn items(path: &str, ops: &[Op], caller: &Caller) -> Result<Vec<Item>> {
    if ops.len() > 1000 {
        return Err(Error::BadRequest("at most 1000 operations per change".into()));
    }
    let owner = |t: &Option<SecretType>| -> Result<String> {
        match t.unwrap_or_default() {
            SecretType::Shared => Ok(String::new()),
            SecretType::Personal if caller.personal() => Ok(caller.principal.clone()),
            SecretType::Personal => Err(Error::BadRequest(format!("{} can't own personal values", caller.principal))),
        }
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let item = match op {
            Op::Set { key, value, comment, tags, kind, link, path: p } => {
                names::key(key)?;
                if let Some(v) = value {
                    names::value(v)?;
                }
                if let Some(c) = comment {
                    names::comment(c)?;
                }
                if let Some(t) = tags {
                    names::tags(t)?;
                }
                let owner = owner(kind)?;
                if !owner.is_empty() && matches!(link, Some(Some(_))) {
                    return Err(Error::BadRequest("a personal value can't be linked".into()));
                }
                Item {
                    path: names::path(p.as_deref().unwrap_or(path))?,
                    key: key.clone(),
                    owner,
                    action: ItemAction::Set { value: value.clone().map(Value::new), comment: comment.clone(), tags: tags.clone(), link: link.clone(), link_state: None },
                }
            }
            Op::Delete { key, kind, path: p } => {
                names::key(key)?;
                Item { path: names::path(p.as_deref().unwrap_or(path))?, key: key.clone(), owner: owner(kind)?, action: ItemAction::Delete }
            }
        };
        if !seen.insert((item.path.clone(), item.key.clone(), item.owner.clone())) {
            return Err(Error::BadRequest(format!("{} appears twice in one change", item.key)));
        }
        out.push(item);
    }
    Ok(out)
}

/// A row of `secrets`, as a write sees it.
#[derive(Clone, Debug)]
pub(crate) struct Current {
    pub id: String,
    pub version: i64,
    pub value: Option<Vec<u8>>,
    pub fp: String,
    pub comment: String,
    pub tags: Vec<String>,
    pub link: Option<Link>,
    pub link_state: Option<Json>,
}

fn current(c: &Conn<'_>, env_id: &str, path: &str, key: &str, owner: &str) -> Result<Option<Current>> {
    Ok(c.query_row(
        "SELECT id, version, value, fp, comment, tags, link, link_state FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ?",
        [env_id, path, key, owner],
        |r| {
            Ok(Current {
                id: r.get(0)?,
                version: r.get(1)?,
                value: r.get(2)?,
                fp: r.get(3)?,
                comment: r.get(4)?,
                tags: from_json(&r.get::<_, String>(5)?),
                link: r.get::<_, Option<String>>(6)?.and_then(|s| serde_json::from_str(&s).ok()),
                link_state: r.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str(&s).ok()),
            })
        },
    )
    .optional()?)
}

fn credential_id(state: &Option<Json>) -> Option<String> {
    state.as_ref()?.get("credential")?.as_str().map(Into::into)
}

/// Apply `items` to `env` as one change. Inside a write closure: only the database.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(c: &Conn<'_>, dek: &Dek, env: &EnvRow, items: &[Item], kind: &str, message: &str, caller: &Caller, now: i64) -> Result<(Change, Vec<ItemDiff>)> {
    let project = &env.project_id;
    c.execute("UPDATE projects SET seq = seq + 1 WHERE project_id = ?", [project])?;
    let seq: i64 = c.query_row("SELECT seq FROM projects WHERE project_id = ?", [project], |r| r.get(0))?;
    let mut diffs = vec![];
    let mut dropped_credentials: BTreeMap<String, (Link, Json)> = BTreeMap::new();
    for item in items {
        let cur = current(c, &env.id, &item.path, &item.key, &item.owner)?;
        let kind_of = if item.owner.is_empty() { SecretType::Shared } else { SecretType::Personal };
        match &item.action {
            ItemAction::Delete => {
                let cur = cur.ok_or_else(|| Error::NotFound(format!("no secret {} in {}:{}", item.key, env.slug, item.path)))?;
                c.execute(
                    "INSERT INTO versions (secret_id, version, seq, project_id, env_id, path, key, owner, op, what, value, fp, comment, tags, link, at, by)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'delete', 'deleted', NULL, NULL, ?, ?, ?, ?, ?)",
                    params![cur.id, cur.version + 1, seq, project, env.id, item.path, item.key, item.owner, cur.comment, json(&cur.tags), cur.link.as_ref().map(json), now, caller.principal],
                )?;
                c.execute("DELETE FROM secrets WHERE id = ?", [&cur.id])?;
                if let (Some(l), Some(id)) = (&cur.link, credential_id(&cur.link_state)) {
                    dropped_credentials.insert(id, (l.clone(), cur.link_state.clone().unwrap_or_default()));
                }
                diffs.push(ItemDiff { key: item.key.clone(), path: item.path.clone(), kind: kind_of, what: vec!["deleted".into()], from_fp: Some(cur.fp), to_fp: None });
            }
            ItemAction::Set { value, comment, tags, link, link_state } => {
                let link_now: Option<Link> = match link {
                    Some(l) => l.clone(),
                    None => cur.as_ref().and_then(|c| c.link.clone()),
                };
                let link_changed = cur.as_ref().map(|c| c.link.clone()) != Some(link_now.clone());
                let state: Option<Json> = match (link_state, link_changed) {
                    (Some(s), _) => Some(s.clone()),
                    (None, false) => cur.as_ref().and_then(|c| c.link_state.clone()),
                    (None, true) => None,
                };
                let stored_value: Option<Value> = match &link_now {
                    Some(l) if l.mode != Mode::Managed => None,
                    _ => match (value, &cur) {
                        (Some(v), _) => Some(v.clone()),
                        (None, Some(cur)) if cur.value.is_some() && !link_changed => Some(dek.open(&value_aad(project, &cur.id, cur.version), cur.value.as_ref().expect("value"))?),
                        (None, _) if link_now.as_ref().is_some_and(|l| l.mode == Mode::Managed) => {
                            return Err(Error::Internal(format!("{}: the managed credential wasn't issued", item.key)));
                        }
                        (None, _) => Some(Value::default()),
                    },
                };
                let fp = match (&link_now, &stored_value) {
                    (Some(l), None) => dek.fingerprint(&l.fingerprint()),
                    (_, Some(v)) => dek.fingerprint(v.expose()),
                    (None, None) => dek.fingerprint(""),
                };
                let comment_now = comment.clone().or_else(|| cur.as_ref().map(|c| c.comment.clone())).unwrap_or_default();
                let tags_now = tags.clone().or_else(|| cur.as_ref().map(|c| c.tags.clone())).unwrap_or_default();
                let mut what = vec![];
                match &cur {
                    None => what.push("added".to_string()),
                    Some(cur) => {
                        if cur.fp != fp {
                            what.push("changed".into());
                        }
                        if cur.comment != comment_now {
                            what.push("comment".into());
                        }
                        if cur.tags != tags_now {
                            what.push("tags".into());
                        }
                        if link_changed {
                            what.push("link".into());
                        }
                        if what.is_empty() && state != cur.link_state {
                            what.push("rotated".into());
                        }
                    }
                }
                if what.is_empty() {
                    continue;
                }
                if let Some(cur) = &cur
                    && let Some(old) = credential_id(&cur.link_state)
                    && credential_id(&state).as_deref() != Some(old.as_str())
                    && let Some(l) = &cur.link
                {
                    dropped_credentials.insert(old, (l.clone(), cur.link_state.clone().unwrap_or_default()));
                }
                let (id, version) = match &cur {
                    Some(cur) => (cur.id.clone(), cur.version + 1),
                    None => c
                        .query_row(
                            "SELECT secret_id, MAX(version) FROM versions WHERE env_id = ? AND path = ? AND key = ? AND owner = ? GROUP BY secret_id ORDER BY MAX(seq) DESC LIMIT 1",
                            [&env.id, &item.path, &item.key, &item.owner],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? + 1)),
                        )
                        .optional()?
                        .unwrap_or_else(|| (names::new_id("sec", 16), 1)),
                };
                let ct = stored_value.as_ref().map(|v| dek.seal(&value_aad(project, &id, version), v));
                let link_json = link_now.as_ref().map(json);
                let tags_json = json(&tags_now);
                let state_json = state.as_ref().map(json);
                match &cur {
                    Some(_) => {
                        c.execute(
                            "UPDATE secrets SET version = ?, value = ?, fp = ?, comment = ?, tags = ?, link = ?, link_state = ?, updated_at = ?, updated_by = ? WHERE id = ?",
                            params![version, ct, fp, comment_now, tags_json, link_json, state_json, now, caller.principal, id],
                        )?;
                    }
                    None => {
                        c.execute(
                            "INSERT INTO secrets (id, project_id, env_id, path, key, owner, version, value, fp, comment, tags, link, link_state, updated_at, updated_by)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                            params![id, project, env.id, item.path, item.key, item.owner, version, ct, fp, comment_now, tags_json, link_json, state_json, now, caller.principal],
                        )?;
                        envs::ensure_folders(c, &env.id, &item.path, now)?;
                    }
                }
                c.execute(
                    "INSERT INTO versions (secret_id, version, seq, project_id, env_id, path, key, owner, op, what, value, fp, comment, tags, link, at, by)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'set', ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![id, version, seq, project, env.id, item.path, item.key, item.owner, what.join(","), ct, fp, comment_now, tags_json, link_json, now, caller.principal],
                )?;
                diffs.push(ItemDiff { key: item.key.clone(), path: item.path.clone(), kind: kind_of, what, from_fp: cur.map(|c| c.fp), to_fp: Some(fp) });
            }
        }
    }
    // A managed credential no secret uses any more keeps working for its grace period, then
    // the sweeper revokes it.
    for (cred, (link, state)) in dropped_credentials {
        let still: i64 = c.query_row("SELECT COUNT(*) FROM secrets WHERE env_id = ? AND link_state LIKE ?", [env.id.as_str(), &format!("%\"credential\":\"{cred}\"%")], |r| r.get(0))?;
        if still == 0 && let Some(handle) = state.get("handle") {
            retire(c, env, &link, handle, &caller.principal, now + link.grace_minutes() as i64 * 60_000, now)?;
        }
    }
    let path = match items.iter().map(|i| i.path.as_str()).collect::<BTreeSet<_>>() {
        s if s.len() == 1 => s.into_iter().next().unwrap_or("/").to_string(),
        _ => "/".to_string(),
    };
    let change = Change { seq, environment: env.slug.clone(), path, kind: kind.into(), message: message.into(), by: caller.principal.clone(), at: now, keys: diffs.len() as i64 };
    c.execute(
        "INSERT INTO changes (project_id, seq, env_id, path, kind, message, by, at, keys) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![project, seq, env.id, change.path, kind, message, caller.principal, now, change.keys],
    )?;
    let keys: Vec<String> = diffs.iter().map(|d| d.key.clone()).collect();
    let summary = summarize(&diffs);
    audit::record(c, project, caller, &format!("secrets.{kind}"), Some(&env.slug), Some(&change.path), &keys, &format!("#{seq} {summary}"), now)?;
    Ok((change, diffs))
}

/// A credential no secret holds any more: revoked by the sweeper at `at`.
pub(crate) fn retire(c: &Conn<'_>, env: &EnvRow, link: &Link, handle: &Json, by: &str, at: i64, now: i64) -> Result<()> {
    c.execute(
        "INSERT INTO leases (id, project_id, env_id, key, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES (?, ?, ?, '', 'retired', ?, ?, ?, ?, ?, NULL)",
        params![names::new_id("lse", 16), env.project_id, env.id, json(link), json(handle), by, now, at],
    )?;
    Ok(())
}

pub(crate) fn summarize(diffs: &[ItemDiff]) -> String {
    let count = |w: &str| diffs.iter().filter(|d| d.what.first().map(String::as_str) == Some(w)).count();
    let (added, deleted) = (count("added"), count("deleted"));
    let changed = diffs.len() - added - deleted;
    let mut parts = vec![];
    for (n, w) in [(added, "added"), (changed, "changed"), (deleted, "deleted")] {
        if n > 0 {
            parts.push(format!("{n} {w}"));
        }
    }
    if parts.is_empty() { "no changes".into() } else { parts.join(", ") }
}

/// The `seq` of the environment's last change.
pub(crate) fn last_seq(c: &Conn<'_>, env_id: &str) -> Result<i64> {
    Ok(c.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes WHERE env_id = ?", [env_id], |r| r.get(0))?)
}

fn change_row(r: &code_sql::Row<'_>) -> code_sql::Result<Change> {
    Ok(Change { seq: r.get(0)?, environment: r.get(1)?, path: r.get(2)?, kind: r.get(3)?, message: r.get(4)?, by: r.get(5)?, at: r.get(6)?, keys: r.get(7)? })
}

pub(crate) const CHANGE_COLUMNS: &str = "c.seq, e.slug, c.path, c.kind, c.message, c.by, c.at, c.keys";

/// The answer of a single-key write.
#[derive(Debug, Serialize)]
pub struct Written {
    pub change: Option<Change>,
    pub secret: Option<crate::read::Secret>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Applied {
    pub change: Option<Change>,
    pub items: Vec<ItemDiff>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Rollback {
    pub to: Option<i64>,
    pub at: Option<i64>,
    pub message: Option<String>,
    #[serde(default)]
    pub bypass: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Promote {
    pub path: Option<String>,
    pub keys: Option<Vec<String>>,
    pub message: Option<String>,
}

impl Secrets {
    /// Issue the managed credentials `items` link to (outside any transaction), and check every
    /// link: its kind, field, mode, and that the resource is in the project.
    pub(crate) fn prepare(&self, project: &str, env_slug: &str, items: &[Item], caller: &Caller) -> Result<Prepared> {
        let _ = caller;
        let mut out = Prepared { items: items.to_vec(), fresh: vec![] };
        let wants_link = items.iter().any(|i| matches!(&i.action, ItemAction::Set { link: Some(Some(_)), .. }));
        if !wants_link {
            return Ok(out);
        }
        let resources: BTreeSet<(String, String)> = self
            .workspace
            .get(project)
            .map_err(|e| Error::Internal(e.to_string()))?
            .resources
            .into_iter()
            .map(|r| (r.kind.as_str().to_string(), r.r#ref))
            .collect();
        let dek = self.project(project)?;
        // Existing credentials of the environment, by (kind, ref, credential): a new secret that
        // links to one shares it.
        let existing: Vec<(Link, Json)> = self.db.read(|c| {
            let env = envs::by_slug(c, project, env_slug);
            let Ok(env) = env else { return Ok::<_, Error>(vec![]) };
            Ok(c.query("SELECT link, link_state FROM secrets WHERE env_id = ? AND link_state IS NOT NULL", [&env.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .into_iter()
                .filter_map(|(l, s)| Some((serde_json::from_str(&l).ok()?, serde_json::from_str(&s).ok()?)))
                .collect())
        })?;
        // Credentials issued or shared in this batch, by (kind, ref, credential).
        let mut issued: BTreeMap<(String, String, String), Credential> = BTreeMap::new();
        let mut result = Ok(());
        for item in &mut out.items {
            let ItemAction::Set { link: Some(Some(link)), value, .. } = &item.action else { continue };
            let (link, has_value) = (link.clone(), value.is_some());
            if let Err(e) = self.links.check(&link) {
                result = Err(e);
                break;
            }
            if !resources.contains(&(link.kind.clone(), link.r#ref.clone())) {
                result = Err(Error::BadRequest(format!("{} {} isn't in this project", link.kind, link.r#ref)));
                break;
            }
            if link.mode != Mode::Managed || has_value {
                continue;
            }
            // The same link on an existing secret: keep its credential.
            let same = self.db.read(|c| {
                let Ok(env) = envs::by_slug(c, project, env_slug) else { return Ok::<_, Error>(false) };
                Ok(current(c, &env.id, &item.path, &item.key, &item.owner)?.is_some_and(|cur| cur.link.as_ref() == Some(&link)))
            })?;
            if same {
                if let ItemAction::Set { link, .. } = &mut item.action {
                    *link = None;
                }
                continue;
            }
            let cred = self.links.credential(&link).unwrap_or("default").to_string();
            let group = (link.kind.clone(), link.r#ref.clone(), cred.clone());
            if !issued.contains_key(&group) {
                let shared = existing.iter().find(|(l, _)| l.kind == link.kind && l.r#ref == link.r#ref && l.mode == Mode::Managed && self.links.credential(l) == Some(cred.as_str()));
                let entry = match shared {
                    Some((_, state)) => (open_parts(&dek, project, state)?, state.clone()),
                    None => {
                        let label = format!("roll secrets {env_slug} {}", item.key);
                        match self.links.get(&link.kind).and_then(|p| p.issue(&link.r#ref, &cred, &link, &label, None)) {
                            Ok(i) => {
                                out.fresh.push(Fresh { kind: link.kind.clone(), r#ref: link.r#ref.clone(), handle: i.handle.clone() });
                                let state = seal_parts(&dek, project, &i.parts, i.handle, self.db.now_ms()?);
                                (i.parts, state)
                            }
                            Err(e) => {
                                result = Err(e);
                                break;
                            }
                        }
                    }
                };
                issued.insert(group.clone(), entry);
            }
            let (parts, state) = &issued[&group];
            let Some(v) = parts.get(&link.field) else {
                result = Err(Error::Link(format!("{} didn't issue {}", link.kind, link.field)));
                break;
            };
            if let ItemAction::Set { value, link_state, .. } = &mut item.action {
                *value = Some(v.clone());
                *link_state = Some(state.clone());
            }
        }
        if let Err(e) = result {
            self.discard(&out);
            return Err(e);
        }
        Ok(out)
    }

    /// Revoke what [`Secrets::prepare`] issued, after the write failed.
    pub(crate) fn discard(&self, p: &Prepared) {
        for f in &p.fresh {
            if let Ok(provider) = self.links.get(&f.kind)
                && let Err(e) = provider.revoke(&f.r#ref, &f.handle)
            {
                tracing::warn!(kind = %f.kind, "revoking an unused credential failed: {e}");
            }
        }
    }

    /// Prepare and apply `items` to `env_slug` as one change, refusing a protected environment
    /// unless `force` (an approved request, a rotation).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit(&self, project: &str, env_slug: &str, items: Vec<Item>, kind: &str, message: &str, if_seq: Option<i64>, force: bool, caller: &Caller) -> Result<Applied> {
        let warnings = self.warnings(project, env_slug, &items, caller);
        let prepared = self.prepare(project, env_slug, &items, caller)?;
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let res = self.db.write(|c| {
            let env = envs::by_slug(c, project, env_slug)?;
            if env.protected && !force {
                return Err(Error::Conflict("approval_required", format!("{} is protected: open a change request", env.slug)));
            }
            if let Some(s) = if_seq
                && last_seq(c, &env.id)? > s
            {
                return Err(Error::Conflict("stale", format!("{} changed after #{s}", env.slug)));
            }
            apply(c, &dek, &env, &prepared.items, kind, message, caller, now)
        });
        match res {
            Ok((change, items)) => Ok(Applied { change: (!items.is_empty()).then_some(change), items, warnings }),
            Err(e) => {
                self.discard(&prepared);
                Err(e)
            }
        }
    }

    fn check_items(&self, project: &str, env: &str, items: &[Item], caller: &Caller) -> Result<()> {
        for i in items {
            let action = match i.action {
                ItemAction::Delete => Action::Delete,
                ItemAction::Set { .. } => Action::Write,
            };
            caller.need(action, &Scope::at(project, env, &i.path))?;
        }
        Ok(())
    }

    pub fn batch(&self, project: &str, env: &str, b: Batch, caller: &Caller) -> Result<Applied> {
        let path = names::path(b.path.as_deref().unwrap_or("/"))?;
        caller.need(Action::Write, &Scope::at(project, env, &path))?;
        let message = b.message.unwrap_or_default();
        names::message(&message)?;
        let items = items(&path, &b.ops, caller)?;
        self.check_items(project, env, &items, caller)?;
        self.commit(project, env, items, "write", &message, b.if_seq, false, caller)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_secret(&self, project: &str, env: &str, path: &str, key: &str, op: Op, message: Option<String>, caller: &Caller) -> Result<Written> {
        let path = names::path(path)?;
        let a = self.batch(project, env, Batch { path: Some(path.clone()), message, ops: vec![op], if_seq: None }, caller)?;
        let kind = a.items.first().map(|d| d.kind).unwrap_or_default();
        let secret = self.secret(project, env, &path, key, Some(kind), caller).ok();
        Ok(Written { change: a.change, secret, warnings: a.warnings })
    }

    pub fn delete_secret(&self, project: &str, env: &str, path: &str, key: &str, kind: Option<SecretType>, caller: &Caller) -> Result<Written> {
        let path = names::path(path)?;
        caller.need(Action::Delete, &Scope::at(project, env, &path))?;
        let items = items(&path, &[Op::Delete { key: key.into(), kind, path: None }], caller)?;
        let a = self.commit(project, env, items, "write", &format!("deleted {key}"), None, false, caller)?;
        Ok(Written { change: a.change, secret: None, warnings: vec![] })
    }

    /// Bring the environment back to its state after change `to` (or at time `at`), as one new
    /// change.
    pub fn rollback(&self, project: &str, slug: &str, r: Rollback, caller: &Caller) -> Result<Applied> {
        caller.need(Action::Write, &Scope::env(project, slug))?;
        caller.need(Action::Reveal, &Scope::env(project, slug))?;
        if r.bypass {
            caller.need(Action::Admin, &Scope::env(project, slug))?;
        }
        let dek = self.project(project)?;
        let (target, items) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let target = match (r.to, r.at) {
                (Some(s), None) => s,
                (None, Some(at)) => c.query_row("SELECT COALESCE(MAX(seq), 0) FROM changes WHERE project_id = ? AND at <= ?", params![project, at], |r| r.get(0))?,
                _ => return Err(Error::BadRequest("rollback takes to (a change) or at (a time)".into())),
            };
            let state = state_at(c, &env.id, target)?;
            // (path, key, owner, fp, comment, tags, link) of every key now.
            #[allow(clippy::type_complexity)]
            let now: Vec<(String, String, String, String, String, String, Option<String>)> = c.query(
                "SELECT path, key, owner, fp, comment, tags, link FROM secrets WHERE env_id = ?",
                [&env.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )?;
            let mut items = vec![];
            for v in &state {
                let same = now.iter().any(|n| n.0 == v.path && n.1 == v.key && n.2 == v.owner && n.3 == v.fp && n.4 == v.comment && n.5 == v.tags && n.6 == v.link);
                if same {
                    continue;
                }
                let link: Option<Link> = v.link.as_deref().and_then(|l| serde_json::from_str(l).ok());
                let value = match &v.value {
                    Some(ct) => Some(dek.open(&value_aad(project, &v.secret_id, v.version), ct)?),
                    None => None,
                };
                items.push(Item {
                    path: v.path.clone(),
                    key: v.key.clone(),
                    owner: v.owner.clone(),
                    action: ItemAction::Set { value, comment: Some(v.comment.clone()), tags: Some(from_json(&v.tags)), link: Some(link), link_state: None },
                });
            }
            for n in &now {
                if !state.iter().any(|v| v.path == n.0 && v.key == n.1 && v.owner == n.2) {
                    items.push(Item { path: n.0.clone(), key: n.1.clone(), owner: n.2.clone(), action: ItemAction::Delete });
                }
            }
            Ok((target, items))
        })?;
        let message = r.message.unwrap_or_else(|| format!("rolled back to #{target}"));
        names::message(&message)?;
        let res = self.commit(project, slug, items, "rollback", &message, None, r.bypass, caller)?;
        if r.bypass {
            self.audit_now(project, caller, "approval.bypass", Some(slug), None, &[], &format!("rolled back to #{target} without approval"));
        }
        Ok(res)
    }

    /// Copy keys from environment `from` into `to` (values, comments, tags, links) as one
    /// change; a managed link gets its own credential in `to`.
    pub fn promote(&self, project: &str, to: &str, from: &str, p: Promote, caller: &Caller) -> Result<Applied> {
        let path = names::path(p.path.as_deref().unwrap_or("/"))?;
        caller.need(Action::Reveal, &Scope::at(project, from, &path))?;
        caller.need(Action::Write, &Scope::at(project, to, &path))?;
        if from == to {
            return Err(Error::BadRequest("promote copies between two environments".into()));
        }
        let items = self.promotion(project, from, to, &path, p.keys.as_deref())?;
        let message = p.message.unwrap_or_else(|| format!("promoted {} from {from}", plural(items.len(), "key")));
        names::message(&message)?;
        self.commit(project, to, items, "promote", &message, None, false, caller)
    }

    /// The items that make `to` match `from` for `keys` (default: every key that differs).
    pub(crate) fn promotion(&self, project: &str, from: &str, to: &str, path: &str, keys: Option<&[String]>) -> Result<Vec<Item>> {
        let dek = self.project(project)?;
        let operator = Caller::operator();
        let source = self.view(project, from, path, &operator, true)?;
        let target = self.view(project, to, path, &operator, true)?;
        let to_env = self.db.read(|c| envs::by_slug(c, project, to))?;
        if to_env.kind == envs::Kind::Ephemeral {
            return Err(Error::BadRequest("an ephemeral environment isn't a promotion target".into()));
        }
        let mut items = vec![];
        for e in source.iter().filter(|e| e.row.owner.is_empty()) {
            if let Some(keys) = keys
                && !keys.contains(&e.row.key)
            {
                continue;
            }
            let t = target.iter().find(|t| t.row.key == e.row.key && t.row.owner.is_empty());
            if keys.is_none() && t.is_some_and(|t| t.row.fp == e.row.fp && t.row.comment == e.row.comment && t.row.tags == e.row.tags) {
                continue;
            }
            let link = e.row.link.clone();
            let value = match (&link, &e.row.value) {
                (Some(l), _) if l.mode == Mode::Managed => None,
                (_, Some(ct)) => Some(dek.open(&value_aad(project, &e.row.id, e.row.version), ct)?),
                _ => None,
            };
            items.push(Item {
                path: path.to_string(),
                key: e.row.key.clone(),
                owner: String::new(),
                action: ItemAction::Set { value, comment: Some(e.row.comment.clone()), tags: Some(e.row.tags.clone()), link: Some(link), link_state: None },
            });
        }
        if let Some(keys) = keys
            && let Some(k) = keys.iter().find(|k| !items.iter().any(|i| &i.key == *k))
        {
            return Err(Error::NotFound(format!("no secret {k} in {from}:{path}")));
        }
        Ok(items)
    }

    pub fn changes(&self, project: &str, slug: &str, before: Option<i64>, limit: Option<i64>, caller: &Caller) -> Result<Vec<Change>> {
        caller.need(Action::Read, &Scope::env(project, slug))?;
        self.project(project)?;
        let limit = limit.unwrap_or(50).clamp(1, 500);
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            Ok(c.query(
                &format!("SELECT {CHANGE_COLUMNS} FROM changes c JOIN environments e ON e.id = c.env_id WHERE c.env_id = ? AND c.seq < ? ORDER BY c.seq DESC LIMIT ?"),
                params![env.id, before.unwrap_or(i64::MAX), limit],
                change_row,
            )?)
        })
    }

    /// A change with what it did to each key; with `state`, every key as it was after it.
    pub fn change(&self, project: &str, slug: &str, seq: i64, with_state: bool, caller: &Caller) -> Result<serde_json::Value> {
        caller.need(Action::Read, &Scope::env(project, slug))?;
        self.project(project)?;
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let change = c
                .query_row(&format!("SELECT {CHANGE_COLUMNS} FROM changes c JOIN environments e ON e.id = c.env_id WHERE c.env_id = ? AND c.seq = ?"), params![env.id, seq], change_row)
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("no change #{seq} in {slug}")))?;
            let items = c.query(
                "SELECT v.key, v.path, v.owner, v.what, v.fp, (SELECT p.fp FROM versions p WHERE p.secret_id = v.secret_id AND p.version = v.version - 1)
                 FROM versions v WHERE v.env_id = ? AND v.seq = ? ORDER BY v.path, v.key",
                params![env.id, seq],
                |r| {
                    let owner: String = r.get(2)?;
                    let what: String = r.get(3)?;
                    Ok(ItemDiff {
                        key: r.get(0)?,
                        path: r.get(1)?,
                        kind: if owner.is_empty() { SecretType::Shared } else { SecretType::Personal },
                        what: what.split(',').map(Into::into).collect(),
                        to_fp: r.get(4)?,
                        from_fp: r.get(5)?,
                    })
                },
            )?;
            let mut out = serde_json::to_value(&change).expect("JSON");
            out["items"] = serde_json::to_value(&items).expect("JSON");
            if with_state {
                let state: Vec<Json> = state_at(c, &env.id, seq)?
                    .into_iter()
                    .filter(|v| v.owner.is_empty())
                    .map(|v| serde_json::json!({ "key": v.key, "path": v.path, "version": v.version, "fp": v.fp, "comment": v.comment, "tags": from_json::<Vec<String>>(&v.tags), "link": v.link.as_deref().and_then(|l| serde_json::from_str::<Json>(l).ok()), "at": v.at }))
                    .collect();
                out["state"] = Json::Array(state);
            }
            Ok(out)
        })
    }
}

pub(crate) struct StateRow {
    pub secret_id: String,
    pub version: i64,
    pub path: String,
    pub key: String,
    pub owner: String,
    pub value: Option<Vec<u8>>,
    pub fp: String,
    pub comment: String,
    pub tags: String,
    pub link: Option<String>,
    pub at: i64,
}

/// Every key of an environment as it was after change `seq`.
pub(crate) fn state_at(c: &Conn<'_>, env_id: &str, seq: i64) -> Result<Vec<StateRow>> {
    let rows = c.query(
        "SELECT v.secret_id, v.version, v.op, v.path, v.key, v.owner, v.value, v.fp, v.comment, v.tags, v.link, v.at FROM versions v
         JOIN (SELECT path, key, owner, MAX(seq) AS top FROM versions WHERE env_id = ? AND seq <= ? GROUP BY path, key, owner) t
           ON t.path = v.path AND t.key = v.key AND t.owner = v.owner AND v.seq = t.top
         WHERE v.env_id = ? ORDER BY v.path, v.key",
        params![env_id, seq, env_id],
        |r| {
            let op: String = r.get(2)?;
            Ok((op, StateRow {
                secret_id: r.get(0)?,
                version: r.get(1)?,
                path: r.get(3)?,
                key: r.get(4)?,
                owner: r.get(5)?,
                value: r.get(6)?,
                fp: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                comment: r.get(8)?,
                tags: r.get(9)?,
                link: r.get(10)?,
                at: r.get(11)?,
            }))
        },
    )?;
    Ok(rows.into_iter().filter(|(op, _)| op == "set").map(|(_, r)| r).collect())
}

/// A managed credential's parts sealed into its link state.
pub(crate) fn seal_parts(dek: &Dek, project: &str, parts: &BTreeMap<String, Value>, handle: Json, now: i64) -> Json {
    let id = names::new_id("crd", 16);
    let plain: BTreeMap<&str, &str> = parts.iter().map(|(k, v)| (k.as_str(), v.expose())).collect();
    let plain = Value::new(serde_json::to_string(&plain).expect("JSON"));
    let sealed = STANDARD.encode(dek.seal(&format!("roll-secrets:credential:{project}:{id}"), &plain));
    serde_json::json!({ "credential": id, "handle": handle, "issued_at": now, "sealed": sealed })
}

pub(crate) fn open_parts(dek: &Dek, project: &str, state: &Json) -> Result<BTreeMap<String, Value>> {
    let id = state.get("credential").and_then(Json::as_str).ok_or_else(|| Error::Internal("a credential without an id".into()))?;
    let sealed = state.get("sealed").and_then(Json::as_str).and_then(|s| STANDARD.decode(s).ok()).ok_or_else(|| Error::Internal("a credential without its parts".into()))?;
    let plain = dek.open(&format!("roll-secrets:credential:{project}:{id}"), &sealed)?;
    let parts: BTreeMap<String, String> = serde_json::from_str(plain.expose()).map_err(|_| Error::Internal("a credential's parts aren't JSON".into()))?;
    Ok(parts.into_iter().map(|(k, v)| (k, Value::new(v))).collect())
}

pub(crate) fn plural(n: usize, w: &str) -> String {
    format!("{n} {w}{}", if n == 1 { "" } else { "s" })
}
