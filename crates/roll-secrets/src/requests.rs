//! Change requests (docs/secrets.md, "Protected environments and approvals"): a batch that waits
//! for approvals, with its values encrypted, applied as one change.

use crate::access::{Action, Caller, Scope};
use crate::crypto::{Dek, request_aad};
use crate::envs::{self, EnvRow};
use crate::links::Link;
use crate::write::{Batch, Item, ItemAction, ItemDiff, SecretType, items};
use crate::{Error, Result, Secrets, Value, audit, from_json, json, names};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct StoredOp {
    path: String,
    key: String,
    owner: String,
    delete: bool,
    value: Option<String>,
    comment: Option<String>,
    tags: Option<Vec<String>>,
    #[serde(default)]
    link: Option<Option<Link>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Review {
    pub by: String,
    pub verdict: String,
    pub comment: String,
    pub at: i64,
}

#[derive(Debug, Serialize)]
pub struct Request {
    pub id: String,
    pub environment: String,
    pub path: String,
    pub message: String,
    pub by: String,
    pub at: i64,
    pub state: String,
    pub items: Vec<ItemDiff>,
    pub reviews: Vec<Review>,
    pub approvals: i64,
    pub approvals_needed: i64,
    pub applied_seq: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub verdict: String,
    #[serde(default)]
    pub comment: String,
}

struct RequestRow {
    id: String,
    path: String,
    message: String,
    by: String,
    at: i64,
    base_seq: i64,
    state: String,
    ops: Vec<u8>,
    items: String,
    reviews: String,
    applied_seq: Option<i64>,
}

const COLUMNS: &str = "id, env_id, path, message, by, at, base_seq, state, ops, items, reviews, applied_seq";

fn request_row(r: &code_sql::Row<'_>) -> code_sql::Result<RequestRow> {
    Ok(RequestRow {
        id: r.get(0)?,
        path: r.get(2)?,
        message: r.get(3)?,
        by: r.get(4)?,
        at: r.get(5)?,
        base_seq: r.get(6)?,
        state: r.get(7)?,
        ops: r.get(8)?,
        items: r.get(9)?,
        reviews: r.get(10)?,
        applied_seq: r.get(11)?,
    })
}

fn approvals(reviews: &[Review], author: &str) -> i64 {
    let mut by: Vec<&str> = reviews.iter().filter(|r| r.verdict == "approve" && r.by != author).map(|r| r.by.as_str()).collect();
    by.sort();
    by.dedup();
    by.len() as i64
}

fn view(env: &EnvRow, r: RequestRow) -> Request {
    let reviews: Vec<Review> = from_json(&r.reviews);
    Request {
        approvals: approvals(&reviews, &r.by),
        approvals_needed: env.approvals.max(1),
        id: r.id,
        environment: env.slug.clone(),
        path: r.path,
        message: r.message,
        by: r.by,
        at: r.at,
        state: r.state,
        items: from_json(&r.items),
        reviews,
        applied_seq: r.applied_seq,
    }
}

fn load(c: &Conn<'_>, env: &EnvRow, id: &str) -> Result<RequestRow> {
    c.query_row(&format!("SELECT {COLUMNS} FROM requests WHERE id = ? AND env_id = ?"), [id, &env.id], request_row).optional()?.ok_or_else(|| Error::NotFound(format!("no request {id}")))
}

/// What `items` would do to `env`, from fingerprints (a request's diff).
fn plan(c: &Conn<'_>, dek: &Dek, env: &EnvRow, items: &[Item]) -> Result<Vec<ItemDiff>> {
    let mut out = vec![];
    for i in items {
        let cur: Option<(String, String, String, Option<String>)> = c
            .query_row("SELECT fp, comment, tags, link FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ?", [&env.id, &i.path, &i.key, &i.owner], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let kind = if i.owner.is_empty() { SecretType::Shared } else { SecretType::Personal };
        let (what, to_fp) = match (&i.action, &cur) {
            (ItemAction::Delete, None) => return Err(Error::NotFound(format!("no secret {} in {}:{}", i.key, env.slug, i.path))),
            (ItemAction::Delete, Some(_)) => (vec!["deleted".to_string()], None),
            (ItemAction::Set { value, comment, tags, link, .. }, cur) => {
                let fp = match (link, value) {
                    (Some(Some(l)), _) if l.mode != crate::Mode::Managed => Some(dek.fingerprint(&l.fingerprint())),
                    (_, Some(v)) => Some(dek.fingerprint(v.expose())),
                    _ => None,
                };
                match cur {
                    None => (vec!["added".to_string()], fp),
                    Some((cfp, ccomment, ctags, clink)) => {
                        let mut w = vec![];
                        if fp.as_ref().is_some_and(|f| f != cfp) {
                            w.push("changed".to_string());
                        }
                        if comment.as_ref().is_some_and(|x| x != ccomment) {
                            w.push("comment".into());
                        }
                        if tags.as_ref().is_some_and(|t| json(t) != *ctags) {
                            w.push("tags".into());
                        }
                        if link.as_ref().is_some_and(|l| l.as_ref().map(json) != *clink) {
                            w.push("link".into());
                        }
                        (w, fp.or_else(|| Some(cfp.clone())))
                    }
                }
            }
        };
        if !what.is_empty() {
            out.push(ItemDiff { key: i.key.clone(), path: i.path.clone(), kind, what, from_fp: cur.map(|c| c.0), to_fp });
        }
    }
    Ok(out)
}

impl Secrets {
    pub fn requests(&self, project: &str, slug: &str, state: Option<&str>, caller: &Caller) -> Result<Vec<Request>> {
        caller.need(Action::Read, &Scope::env(project, slug))?;
        self.project(project)?;
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let rows = c.query(&format!("SELECT {COLUMNS} FROM requests WHERE env_id = ? ORDER BY at DESC LIMIT 200"), [&env.id], request_row)?;
            Ok(rows.into_iter().filter(|r| state.is_none_or(|s| r.state == s) && caller.allows(Action::Read, &Scope::at(project, slug, &r.path))).map(|r| view(&env, r)).collect())
        })
    }

    pub fn request(&self, project: &str, slug: &str, id: &str, caller: &Caller) -> Result<Request> {
        caller.need(Action::Read, &Scope::env(project, slug))?;
        self.project(project)?;
        let r = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            Ok::<_, Error>(view(&env, load(c, &env, id)?))
        })?;
        caller.need(Action::Read, &Scope::at(project, slug, &r.path))?;
        Ok(r)
    }

    pub fn open_request(&self, project: &str, slug: &str, b: Batch, caller: &Caller) -> Result<Request> {
        let path = names::path(b.path.as_deref().unwrap_or("/"))?;
        caller.need(Action::Write, &Scope::at(project, slug, &path))?;
        let message = b.message.unwrap_or_default();
        names::message(&message)?;
        let items = items(&path, &b.ops, caller)?;
        self.check_items(project, slug, &items, caller)?;
        self.open_request_items(project, slug, &path, message, items, caller)
    }

    /// Open a request for `items` (a batch's, or a promotion's into a protected environment).
    pub(crate) fn open_request_items(&self, project: &str, slug: &str, path: &str, message: String, items: Vec<Item>, caller: &Caller) -> Result<Request> {
        let path = path.to_string();
        for i in &items {
            caller.need(if matches!(i.action, ItemAction::Delete) { Action::Delete } else { Action::Write }, &Scope::at(project, slug, &i.path))?;
            if let ItemAction::Set { link: Some(Some(l)), .. } = &i.action {
                self.links.check(l)?;
            }
        }
        if items.is_empty() {
            return Err(Error::BadRequest("a request changes at least one key".into()));
        }
        let dek = self.project(project)?;
        let id = names::new_id("req", 12);
        let ops: Vec<StoredOp> = items
            .iter()
            .enumerate()
            .map(|(n, i)| match &i.action {
                ItemAction::Delete => StoredOp { path: i.path.clone(), key: i.key.clone(), owner: i.owner.clone(), delete: true, value: None, comment: None, tags: None, link: None },
                ItemAction::Set { value, comment, tags, link, .. } => StoredOp {
                    path: i.path.clone(),
                    key: i.key.clone(),
                    owner: i.owner.clone(),
                    delete: false,
                    value: value.as_ref().map(|v| STANDARD.encode(dek.seal(&request_aad(&id, n), v))),
                    comment: comment.clone(),
                    tags: tags.clone(),
                    link: link.clone(),
                },
            })
            .collect();
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let diff = plan(c, &dek, &env, &items)?;
            if diff.is_empty() {
                return Err(Error::BadRequest("the request changes nothing".into()));
            }
            let base_seq: i64 = c.query_row("SELECT seq FROM projects WHERE project_id = ?", [project], |r| r.get(0))?;
            c.execute(
                &format!("INSERT INTO requests ({COLUMNS}, project_id) VALUES (?, ?, ?, ?, ?, ?, ?, 'open', ?, ?, '[]', NULL, ?)"),
                params![id, env.id, path, message, caller.principal, now, base_seq, json(&ops).into_bytes(), json(&diff), project],
            )?;
            let keys: Vec<String> = diff.iter().map(|d| d.key.clone()).collect();
            audit::record(c, project, caller, "request.open", Some(slug), Some(&path), &keys, &format!("opened {id}: {}", crate::write::summarize(&diff)), now)?;
            Ok(view(&env, load(c, &env, &id)?))
        })
    }

    pub fn review_request(&self, project: &str, slug: &str, id: &str, v: Verdict, caller: &Caller) -> Result<Request> {
        caller.need(Action::Admin, &Scope::env(project, slug))?;
        if !["approve", "reject"].contains(&v.verdict.as_str()) {
            return Err(Error::BadRequest("verdict is approve or reject".into()));
        }
        names::comment(&v.comment)?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let r = load(c, &env, id)?;
            if !["open", "approved"].contains(&r.state.as_str()) {
                return Err(Error::Conflict("conflict", format!("{id} is {}", r.state)));
            }
            if r.by == caller.principal {
                return Err(Error::Forbidden("the author of a request can't review it (an admin can apply it with bypass)".into()));
            }
            if !env.approvers.is_empty() && !env.approvers.contains(&caller.principal) {
                return Err(Error::Forbidden(format!("{} isn't an approver of {slug}", caller.principal)));
            }
            let mut reviews: Vec<Review> = from_json(&r.reviews);
            reviews.retain(|x| x.by != caller.principal);
            reviews.push(Review { by: caller.principal.clone(), verdict: v.verdict.clone(), comment: v.comment.clone(), at: now });
            let state = if reviews.iter().any(|x| x.verdict == "reject") {
                "rejected"
            } else if approvals(&reviews, &r.by) >= env.approvals.max(1) {
                "approved"
            } else {
                "open"
            };
            c.execute("UPDATE requests SET reviews = ?, state = ? WHERE id = ?", params![json(&reviews), state, id])?;
            audit::record(c, project, caller, &format!("request.{}", v.verdict), Some(slug), Some(&r.path), &[], &format!("{}d {id}", v.verdict.trim_end_matches('e')), now)?;
            Ok(view(&env, load(c, &env, id)?))
        })
    }

    /// Apply an approved request (or any open one, with `bypass` and `admin`) as one change.
    pub fn apply_request(&self, project: &str, slug: &str, id: &str, bypass: bool, caller: &Caller) -> Result<Request> {
        caller.need(Action::Write, &Scope::env(project, slug))?;
        if bypass {
            caller.need(Action::Admin, &Scope::env(project, slug))?;
        }
        let dek = self.project(project)?;
        let (env, r) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let r = load(c, &env, id)?;
            Ok::<_, Error>((env, r))
        })?;
        match r.state.as_str() {
            "approved" => {}
            "open" if bypass => {}
            "open" => return Err(Error::Conflict("approval_required", format!("{id} has {} of {} approvals", approvals(&from_json::<Vec<Review>>(&r.reviews), &r.by), env.approvals.max(1)))),
            s => return Err(Error::Conflict("conflict", format!("{id} is {s}"))),
        }
        let ops: Vec<StoredOp> = serde_json::from_slice(&r.ops).map_err(|_| Error::Internal("a request's operations aren't JSON".into()))?;
        let mut items = Vec::with_capacity(ops.len());
        for (n, o) in ops.iter().enumerate() {
            let action = if o.delete {
                ItemAction::Delete
            } else {
                let value = match &o.value {
                    Some(b) => Some(dek.open(&request_aad(id, n), &STANDARD.decode(b).map_err(|_| Error::Internal("a pending value isn't base64".into()))?)?),
                    None => None::<Value>,
                };
                ItemAction::Set { value, comment: o.comment.clone(), tags: o.tags.clone(), link: o.link.clone(), link_state: None }
            };
            items.push(Item { path: o.path.clone(), key: o.key.clone(), owner: o.owner.clone(), action });
        }
        let message = if r.message.is_empty() { format!("applied {id}") } else { format!("{} ({id})", r.message) };
        let prepared = self.prepare(project, slug, &items, caller)?;
        let now = self.db.now_ms()?;
        // One transaction: the request is still in the state it was read in, no key it touches
        // changed since it was opened, the change, and the request marked applied.
        let res = self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let current = load(c, &env, id)?;
            if current.state != r.state {
                return Err(Error::Conflict("conflict", format!("{id} is {} now", current.state)));
            }
            for i in &prepared.items {
                let newer: i64 = c.query_row(
                    "SELECT COUNT(*) FROM versions WHERE env_id = ? AND path = ? AND key = ? AND owner = ? AND seq > ?",
                    params![env.id, i.path, i.key, i.owner, r.base_seq],
                    |x| x.get(0),
                )?;
                if newer > 0 {
                    return Err(Error::Conflict("stale", format!("{} changed after {id} was opened: open a new request", i.key)));
                }
            }
            let (change, diffs) = crate::write::apply(c, &dek, &env, &prepared.items, "request", &message, caller, now)?;
            let seq = (!diffs.is_empty()).then_some(change.seq);
            c.execute("UPDATE requests SET state = 'applied', applied_seq = ? WHERE id = ? AND state = ?", params![seq, id, r.state])?;
            if bypass && r.state != "approved" {
                audit::record(c, project, caller, "approval.bypass", Some(slug), Some(&r.path), &[], &format!("applied {id} without approval"), now)?;
            }
            Ok(())
        });
        if let Err(e) = res {
            self.discard(&prepared);
            if matches!(e, Error::Conflict("stale", _)) {
                self.db.write(|c| c.execute("UPDATE requests SET state = 'stale' WHERE id = ? AND state = ?", [id, r.state.as_str()]))?;
            }
            return Err(e);
        }
        self.request(project, slug, id, caller)
    }

    pub fn close_request(&self, project: &str, slug: &str, id: &str, caller: &Caller) -> Result<Request> {
        caller.need(Action::Write, &Scope::env(project, slug))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let r = load(c, &env, id)?;
            if r.by != caller.principal && !caller.allows(Action::Admin, &Scope::env(project, slug)) {
                return Err(Error::Forbidden("only the author or an admin closes a request".into()));
            }
            if ["applied", "closed"].contains(&r.state.as_str()) {
                return Err(Error::Conflict("conflict", format!("{id} is {}", r.state)));
            }
            c.execute("UPDATE requests SET state = 'closed' WHERE id = ?", [id])?;
            audit::record(c, project, caller, "request.close", Some(slug), Some(&r.path), &[], &format!("closed {id}"), now)?;
            Ok(view(&env, load(c, &env, id)?))
        })
    }
}
