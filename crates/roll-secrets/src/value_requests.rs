//! Secret requests (docs/secrets.md, "Asking for a value"): a link that asks someone outside roll
//! (a vendor, a contractor) to send a value in, as Infisical's secret requests do. The other way
//! round from a share.
//!
//! Each request has its own X25519 key pair, made here. The public key is on the request's public
//! page; the secret key is kept sealed with the project's data key (as every value is) and dropped
//! when the request closes. The answerer's browser seals each value to the public key
//! (`roll-seal-v1`, with `roll-secret-request:<id>:<KEY>` as associated data), so what travels and
//! what any proxy or log in between could see is a box. roll opens the boxes only inside the write
//! that lands them: as a change on the folder (by `request:<id>`, naming who asked), or as a change
//! request where the folder needs approval (by whoever asked, so they can't approve it themselves).
//! Either way the answer is claimed in that same transaction, so a request answered once never
//! lands twice, and a refused answer uses nothing up.
//!
//! The answer's destination is roll itself, so unlike a share's value roll must be able to open it
//! in the end; a key only the requester's browser held would add a step (they'd have to open it and
//! write it) without keeping it from roll.

use crate::access::{Action, Caller, Everything, Scope};
use crate::crypto::{self, Sealed};
use crate::write::{Item, ItemAction};
use crate::{Error, Result, Secrets, Value, audit, from_json, json, names};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::sync::Arc;
use zeroize::Zeroizing;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS value_requests (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    keys TEXT NOT NULL,
    note TEXT NOT NULL,
    public_key TEXT NOT NULL,
    seal_key BLOB,
    answers_left INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    answered_at INTEGER,
    cancelled_at INTEGER,
    outcome TEXT
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS value_requests_project ON value_requests (project_id, created_at);
CREATE INDEX IF NOT EXISTS value_requests_expiry ON value_requests (expires_at);
";

/// Keys one request asks for.
const KEYS_MAX: usize = 20;
/// Requests a project may have open at once (as shares).
const OPEN_MAX: i64 = 200;
/// Answers a minute from one address, and to one request (public: a cap on work).
const ANSWERS_PER_IP: u32 = 10;
const ANSWERS_PER_REQUEST: u32 = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewValueRequest {
    /// The keys to send (1–20), in the folder the request is made on.
    pub keys: Vec<String>,
    /// Shown on the answer page ("the Stripe live key for the shop"); never a value.
    #[serde(default)]
    pub note: String,
    /// 5 minutes to 30 days (default 7 days).
    pub ttl_minutes: Option<i64>,
    /// How many times it may be answered, 1–10 (default 1); each answer is its own change.
    pub answers: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ValueRequest {
    pub id: String,
    /// The environment's slug (empty once it's deleted).
    pub environment: String,
    pub path: String,
    pub keys: Vec<String>,
    pub note: String,
    pub answers_left: i64,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub answered_at: Option<i64>,
    pub cancelled_at: Option<i64>,
    /// Whether it can still be answered.
    pub open: bool,
    /// What the last answer became: `{"change": seq}` or `{"request": id}`.
    pub outcome: Option<Json>,
    /// `<dashboard>/request/<id>` when the server knows the dashboard's address (`ROLL_WEB_URL`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// What a request's public page shows: the note, the key names, and the key to seal to.
#[derive(Debug, Serialize)]
pub struct ValuePeek {
    pub note: String,
    pub keys: Vec<String>,
    pub expires_at: i64,
    /// base64url X25519 public key: seal each value to it (`roll-seal-v1`).
    pub public_key: String,
}

/// An answer: one `roll-seal-v1` box per key asked for, each sealed with
/// [`answer_context`] as its associated data.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub values: BTreeMap<String, Sealed>,
}

/// The associated data of an answer's box for `key`: it opens only as that key of that request.
pub fn answer_context(id: &str, key: &str) -> String {
    format!("roll-secret-request:{id}:{key}")
}

/// The associated data of a request's sealed secret key.
fn key_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:value-request:{project}:{id}")
}

fn gone() -> Error {
    Error::Gone("this request was already answered, expired or was cancelled".into())
}

const COLUMNS: &str = "r.id, COALESCE(e.slug, ''), r.path, r.keys, r.note, r.answers_left, r.created_by, r.created_at, r.expires_at, r.answered_at, r.cancelled_at, r.seal_key IS NOT NULL, r.outcome";

fn row(r: &code_sql::Row<'_>, now: i64) -> code_sql::Result<ValueRequest> {
    let (expires_at, left, live): (i64, i64, bool) = (r.get(8)?, r.get(5)?, r.get::<_, i64>(11)? != 0);
    let environment: String = r.get(1)?;
    Ok(ValueRequest {
        id: r.get(0)?,
        open: live && left > 0 && expires_at > now && !environment.is_empty(),
        environment,
        path: r.get(2)?,
        keys: from_json(&r.get::<_, String>(3)?),
        note: r.get(4)?,
        answers_left: left,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        expires_at,
        answered_at: r.get(9)?,
        cancelled_at: r.get(10)?,
        outcome: r.get::<_, Option<String>>(12)?.and_then(|o| serde_json::from_str(&o).ok()),
        link: None,
    })
}

impl Secrets {
    /// Ask for `keys` in `slug`'s `path` (`write` there: the answer is written as the asker).
    pub fn create_value_request(&self, project: &str, slug: &str, path: &str, n: NewValueRequest, caller: &Caller) -> Result<ValueRequest> {
        let path = names::path(path)?;
        caller.need(Action::Write, &Scope::at(project, slug, &path))?;
        let mut keys: Vec<String> = Vec::new();
        for k in n.keys.iter().map(|k| k.trim()) {
            names::key(k)?;
            if !keys.iter().any(|x| x == k) {
                keys.push(k.to_string());
            }
        }
        if keys.is_empty() || keys.len() > KEYS_MAX {
            return Err(Error::BadRequest(format!("a request asks for 1–{KEYS_MAX} keys")));
        }
        names::comment(&n.note)?;
        let ttl = n.ttl_minutes.unwrap_or(7 * 24 * 60);
        let answers = n.answers.unwrap_or(1);
        if !(5..=30 * 24 * 60).contains(&ttl) || !(1..=10).contains(&answers) {
            return Err(Error::BadRequest("ttl_minutes is 5–43200 (30 days) and answers 1–10".into()));
        }
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let id = names::new_id("vrq", 24);
        let (secret, public_key) = crypto::keypair();
        let sealed = dek.seal(&key_aad(project, &id), &Value::new(URL_SAFE_NO_PAD.encode(*secret)));
        let expires_at = now + ttl * 60_000;
        self.db.write(|c| {
            let env = crate::envs::by_slug(c, project, slug)?;
            let open: i64 = c.query_row("SELECT COUNT(*) FROM value_requests WHERE project_id = ? AND seal_key IS NOT NULL AND expires_at > ?", params![project, now], |r| r.get(0))?;
            if open >= OPEN_MAX {
                return Err(Error::Conflict("conflict", format!("{OPEN_MAX} secret requests are open: cancel some first")));
            }
            c.execute(
                "INSERT INTO value_requests (id, project_id, env_id, path, keys, note, public_key, seal_key, answers_left, created_by, created_at, expires_at, answered_at, cancelled_at, outcome)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL)",
                params![id, project, env.id, path, json(&keys), n.note, public_key, sealed, answers, caller.principal, now, expires_at],
            )?;
            let note = if n.note.is_empty() { String::new() } else { format!(": {}", n.note) };
            audit::record(c, project, caller, "value_request.create", Some(slug), Some(&path), &keys, &format!("asked for {} from outside roll ({answers} answer{}, {ttl} min){note}", keys.join(", "), if answers == 1 { "" } else { "s" }), now)?;
            Ok(())
        })?;
        Ok(ValueRequest {
            link: self.web_url.as_ref().map(|w| format!("{}/request/{id}", w.trim_end_matches('/'))),
            id,
            environment: slug.to_string(),
            path,
            keys,
            note: n.note,
            answers_left: answers,
            created_by: caller.principal.clone(),
            created_at: now,
            expires_at,
            answered_at: None,
            cancelled_at: None,
            open: true,
            outcome: None,
        })
    }

    /// The project's secret requests, newest first (never a key).
    pub fn value_requests(&self, project: &str, caller: &Caller) -> Result<Vec<ValueRequest>> {
        caller.need(Action::Read, &Scope::whole(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        Ok(self.db.read(|c| {
            c.query(
                &format!("SELECT {COLUMNS} FROM value_requests r LEFT JOIN environments e ON e.id = r.env_id WHERE r.project_id = ? ORDER BY r.created_at DESC LIMIT 200"),
                [project],
                |r| row(r, now),
            )
        })?)
    }

    /// Cancel a request: its key goes now, and its link answers 410. Its asker's, or an admin's of
    /// its folder.
    pub fn cancel_value_request(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let found = c
                .query_row(&format!("SELECT {COLUMNS} FROM value_requests r LEFT JOIN environments e ON e.id = r.env_id WHERE r.id = ? AND r.project_id = ?"), [id, project], |r| row(r, now))
                .optional()?;
            let r = found.ok_or_else(|| Error::NotFound(format!("no secret request {id}")))?;
            let admin = if r.environment.is_empty() { caller.allows(Action::Admin, &Scope::project(project)) } else { caller.allows(Action::Admin, &Scope::at(project, &r.environment, &r.path)) };
            if r.created_by != caller.principal && !admin {
                return Err(Error::Forbidden("only whoever asked, or an admin, cancels a secret request".into()));
            }
            if c.execute("UPDATE value_requests SET seal_key = NULL, answers_left = 0, cancelled_at = ? WHERE id = ? AND seal_key IS NOT NULL", params![now, id])? == 1 {
                let env = (!r.environment.is_empty()).then_some(r.environment.as_str());
                audit::record(c, project, caller, "value_request.cancel", env, Some(&r.path), &r.keys, &format!("cancelled secret request {id}"), now)?;
            }
            Ok(())
        })
    }

    /// What a request's page shows (public): 404 for none, 410 once it can't be answered.
    pub fn peek_value_request(&self, id: &str) -> Result<ValuePeek> {
        let now = self.db.now_ms()?;
        let found = self.db.read(|c| {
            c.query_row(
                "SELECT r.note, r.keys, r.expires_at, r.public_key, r.seal_key IS NOT NULL AND r.answers_left > 0 AND e.id IS NOT NULL FROM value_requests r LEFT JOIN environments e ON e.id = r.env_id WHERE r.id = ?",
                [id],
                |r| Ok((ValuePeek { note: r.get(0)?, keys: from_json(&r.get::<_, String>(1)?), expires_at: r.get(2)?, public_key: r.get(3)? }, r.get::<_, i64>(4)? != 0)),
            )
            .optional()
        })?;
        let (peek, live) = found.ok_or_else(|| Error::NotFound(format!("no secret request {id}")))?;
        if !live || peek.expires_at <= now {
            return Err(gone());
        }
        Ok(peek)
    }

    /// Answer a request (public): every key it asks for, each in a box sealed to its key. It lands
    /// as a change, or a change request where the folder needs approval, with the answer claimed
    /// in that transaction.
    pub fn answer_value_request(&self, id: &str, a: Answer, ip: Option<String>) -> Result<()> {
        // Before any work: answering is public.
        let busy = !self.answers.allow(&format!("request:{id}"), ANSWERS_PER_REQUEST) || ip.as_ref().is_some_and(|ip| !self.answers.allow(&format!("ip:{ip}"), ANSWERS_PER_IP));
        if busy {
            return Err(Error::TooMany("too many answers: try again in a minute".into()));
        }
        let now = self.db.now_ms()?;
        type Found = (String, String, String, String, String, Option<Vec<u8>>, i64, i64, Option<String>);
        let found: Option<Found> = self.db.read(|c| {
            c.query_row(
                "SELECT r.project_id, r.env_id, r.path, r.keys, r.created_by, r.seal_key, r.answers_left, r.expires_at, e.slug FROM value_requests r LEFT JOIN environments e ON e.id = r.env_id WHERE r.id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )
            .optional()
        })?;
        let (project, env_id, path, keys, requester, sealed_key, left, expires_at, slug) = found.ok_or_else(|| Error::NotFound(format!("no secret request {id}")))?;
        let (Some(sealed_key), Some(slug)) = (sealed_key, slug) else { return Err(gone()) };
        if left <= 0 || expires_at <= now {
            return Err(gone());
        }
        let keys: Vec<String> = from_json(&keys);
        let mut asked = keys.clone();
        asked.sort();
        if !a.values.keys().eq(asked.iter()) {
            return Err(Error::BadRequest(format!("an answer has a value for each key asked for: {}", keys.join(", "))));
        }
        if a.values.values().any(|b| b.ct.len() > crate::shares::CIPHERTEXT_MAX) {
            return Err(Error::BadRequest(format!("a value is at most {} KiB", names::VALUE_MAX / 1024)));
        }
        let dek = self.project(&project)?;
        let secret = dek.open(&key_aad(&project, id), &sealed_key)?;
        let secret: Zeroizing<[u8; 32]> = Zeroizing::new(URL_SAFE_NO_PAD.decode(secret.expose()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| Error::Internal(format!("{id}'s key doesn't decode")))?);
        let mut items = Vec::with_capacity(keys.len());
        for key in &keys {
            let plain = crypto::open_sealed(&secret, &answer_context(id, key), &a.values[key]).map_err(|_| Error::BadRequest(format!("the value for {key} doesn't open: seal it to this request's public key")))?;
            let value = std::str::from_utf8(&plain).map_err(|_| Error::BadRequest(format!("the value for {key} isn't UTF-8 text")))?;
            if value.is_empty() {
                return Err(Error::BadRequest(format!("the value for {key} is empty")));
            }
            names::value(value)?;
            items.push(Item { path: path.clone(), key: key.clone(), owner: String::new(), action: ItemAction::set_value(Value::new(value)) });
        }
        // As whoever asked, as they are now: no answer lands where they couldn't write.
        let landing = self.acting_as(&requester, format!("request:{id}"))?.ok_or_else(|| Error::Gone(format!("{requester} asked for this and can't write secrets any more")))?;
        let landing = Caller { ip: ip.clone(), ..landing };
        self.check_items(&project, &slug, &items, &landing)?;
        let answerer = Caller { principal: "request-link".into(), grants: Arc::new(Everything), ip, runtime: false, approval: None };
        let claim = |c: &Conn<'_>, outcome: Json, summary: String| -> Result<()> {
            let n = c.execute(
                "UPDATE value_requests SET answers_left = answers_left - 1, answered_at = ?, outcome = ?, seal_key = CASE WHEN answers_left <= 1 THEN NULL ELSE seal_key END
                 WHERE id = ? AND seal_key IS NOT NULL AND answers_left > 0 AND expires_at > ?",
                params![now, outcome.to_string(), id, now],
            )?;
            if n != 1 {
                return Err(gone());
            }
            audit::record(c, &project, &answerer, "value_request.answer", Some(&slug), Some(&path), &keys, &summary, now)
        };
        let message = format!("answer to secret request {id}, asked by {requester}");
        let landed = self.commit_hooked(&project, &slug, items.clone(), &[], "write", &message, None, false, &landing, &|c, change| {
            claim(c, serde_json::json!({ "change": change.seq }), format!("answered secret request {id}: change #{} in {slug}", change.seq))?;
            notify_landed(c, self.web_url.as_deref(), &project, &env_id, &slug, &path, id, &requester, &keys, change.seq, now)
        });
        match landed {
            Ok(_) => Ok(()),
            // The folder needs approval: a change request instead, by whoever asked (an approver
            // other than them approves it); its own notice tells the approvers.
            Err(Error::Conflict("approval_required", _)) => {
                let asker = Caller { principal: requester.clone(), ..landing };
                self.open_request_hooked(&project, &slug, &path, message, items, &[], &asker, &|c, request| claim(c, serde_json::json!({ "request": request }), format!("answered secret request {id}: change request {request} in {slug}")))?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Drop the keys of expired requests (the sweeper).
    pub(crate) fn expire_value_requests(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        Ok(self.db.write(|c| c.execute("UPDATE value_requests SET seal_key = NULL WHERE seal_key IS NOT NULL AND expires_at <= ?", [now]))?)
    }
}

/// The environment's notice (its `notify_url`, when it has one) that an answer landed: key names
/// and the change, never a value.
#[allow(clippy::too_many_arguments)]
fn notify_landed(c: &Conn<'_>, web: Option<&str>, project: &str, env_id: &str, slug: &str, path: &str, id: &str, requester: &str, keys: &[String], seq: i64, now: i64) -> Result<()> {
    let notify: Option<i64> = c.query_row("SELECT 1 FROM environments WHERE id = ? AND notify_url IS NOT NULL", [env_id], |r| r.get(0)).optional()?;
    if notify.is_none() {
        return Ok(());
    }
    let link = web.map(|w| format!("{}/dashboard/projects/{project}/secrets?env={slug}", w.trim_end_matches('/')));
    let at = if path == "/" { slug.to_string() } else { format!("{slug}{path}") };
    let mut text = format!("roll secrets: secret request {id}, asked by {requester}, was answered: {} set in {at} (change #{seq}).", keys.join(", "));
    if let Some(l) = &link {
        text.push_str(&format!(" {l}"));
    }
    let slack = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = serde_json::json!({ "text": slack, "content": text, "allowed_mentions": { "parse": [] }, "project": project, "environment": slug, "secret_request": { "id": id, "by": requester, "keys": keys, "path": path, "change": seq }, "link": link });
    crate::notices::queue_body(c, project, env_id, &body, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_the_answer_pages_box() {
        // `apps/dashboard/src/lib/secrets-ask.test.ts`: a fixed ephemeral key and nonce, sealed to
        // the request key whose secret is the bytes 1…32.
        let secret: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
        let b = Sealed { v: 1, epk: "E75P6uryBMf9M1j8nAByGIHRdCeBKCJ-xnTzf3_pe20".into(), nonce: "CQkJCQkJCQkJCQkJ".into(), ct: "1HuLapmEhhMzGSncJaIeQhH3fUE_nFEU".into() };
        assert_eq!(&*crypto::open_sealed(&secret, &answer_context("vrq_test", "VENDOR_KEY"), &b).unwrap(), b"fake-key");
        assert!(crypto::open_sealed(&secret, &answer_context("vrq_test", "OTHER"), &b).is_err());
    }
}
