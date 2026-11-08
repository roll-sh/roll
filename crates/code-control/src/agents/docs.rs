//! Documents the dashboard keeps per owner, so every device (the dashboard on a laptop or phone,
//! the desktop app) shows the same (docs/code/agents.md, "Canvas" and "Owner settings"):
//!
//! - `/v1/code/agents/{owner}/canvas`: the canvas as its owner arranged it.
//! - `/v1/code/agents/{owner}/settings`: the owner's agents settings (T3 Code's client settings
//!   that follow the user: notifications, confirmations, defaults for new threads, …).
//!
//! Each is the dashboard's and opaque here: one JSON object of at most [`Doc::max`] bytes, with a
//! revision that counts its saves. A save takes an owner cursor in the same transaction and goes
//! out on the owner's event stream (`canvas` or `settings`), so open devices load it again and a
//! reconnecting stream catches up on it from the table. Neither holds a credential: the
//! dashboard never puts one there, and roll keeps nothing secret in them.

use super::api::readable;
use super::store::take_cursors;
use crate::{Caller, Code, Error, Result};
use agent_proto::StreamEvent;
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use code_sql::{Conn, OptionalExtension, Row, params};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_canvas (
    owner_id TEXT PRIMARY KEY,
    -- The document, JSON (an object; the dashboard's, opaque to roll).
    doc TEXT NOT NULL,
    -- Saves so far: 1 after the first.
    revision INTEGER NOT NULL,
    -- The owner cursor of the last save (the `canvas` event's).
    cursor INTEGER NOT NULL,
    -- Unix ms.
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS agent_settings (
    owner_id TEXT PRIMARY KEY,
    -- The owner's agents settings, JSON (an object; the dashboard's, opaque to roll).
    doc TEXT NOT NULL,
    revision INTEGER NOT NULL,
    -- The owner cursor of the last save (the `settings` event's).
    cursor INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
"#;

/// Which document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doc {
    Canvas,
    Settings,
}

impl Doc {
    /// Every document, for a stream catching up.
    pub const ALL: [Doc; 2] = [Doc::Canvas, Doc::Settings];

    fn table(self) -> &'static str {
        match self {
            Doc::Canvas => "agent_canvas",
            Doc::Settings => "agent_settings",
        }
    }

    /// Its field in request and response bodies, and its name in messages.
    pub fn field(self) -> &'static str {
        match self {
            Doc::Canvas => "canvas",
            Doc::Settings => "settings",
        }
    }

    /// The largest document, as compact JSON.
    pub fn max(self) -> usize {
        match self {
            Doc::Canvas => 64 << 10,
            Doc::Settings => 32 << 10,
        }
    }

    /// The largest request body: the document and the `{"<field>": …, "revision": …}` around it,
    /// with room for whitespace the client may add.
    fn max_body(self) -> usize {
        self.max() + (4 << 10)
    }

    /// Who may save it besides the owner's own sessions: nobody. The words for the refusal.
    fn verb(self) -> &'static str {
        match self {
            Doc::Canvas => "arrange",
            Doc::Settings => "change",
        }
    }

    fn what(self, owner: &str) -> String {
        match self {
            Doc::Canvas => format!("{owner}'s canvas"),
            Doc::Settings => format!("{owner}'s agents settings"),
        }
    }
}

/// A document as stored.
#[derive(Clone, Debug)]
pub struct Saved {
    pub doc: String,
    pub revision: u64,
    pub cursor: u64,
    pub updated_at: i64,
}

impl Saved {
    pub(crate) fn event(&self, kind: Doc) -> StreamEvent {
        let (cursor, revision, updated_at) = (self.cursor, self.revision, self.updated_at.max(0) as u64);
        match kind {
            Doc::Canvas => StreamEvent::Canvas { cursor, revision, updated_at },
            Doc::Settings => StreamEvent::Settings { cursor, revision, updated_at },
        }
    }
}

fn saved_row(r: &Row<'_>) -> code_sql::Result<Saved> {
    Ok(Saved { doc: r.get(0)?, revision: r.get::<_, i64>(1)?.max(0) as u64, cursor: r.get::<_, i64>(2)?.max(0) as u64, updated_at: r.get(3)? })
}

const COLS: &str = "doc, revision, cursor, updated_at";

pub fn load(db: &code_sql::Db, kind: Doc, owner_id: &str) -> code_sql::Result<Option<Saved>> {
    db.query_row(&format!("SELECT {COLS} FROM {} WHERE owner_id = ?", kind.table()), [owner_id], saved_row).optional()
}

fn load_on(c: &Conn<'_>, kind: Doc, owner_id: &str) -> code_sql::Result<Option<Saved>> {
    c.query_row(&format!("SELECT {COLS} FROM {} WHERE owner_id = ?", kind.table()), [owner_id], saved_row).optional()
}

/// The document if it was saved after `cursor` (an event stream catching up).
pub fn after(db: &code_sql::Db, kind: Doc, owner_id: &str, cursor: u64) -> code_sql::Result<Option<Saved>> {
    db.query_row(&format!("SELECT {COLS} FROM {} WHERE owner_id = ? AND cursor > ?", kind.table()), params![owner_id, cursor as i64], saved_row)
        .optional()
}

pub enum Save {
    Saved(Saved),
    /// `expect` wasn't the revision: what is there now (`None`: nothing yet).
    Conflict(Option<Saved>),
}

/// Save `doc` as the owner's document. With `expect`, only over that revision (0: none saved
/// yet); without, over whatever is there.
pub fn save(db: &code_sql::Db, kind: Doc, owner_id: &str, doc: &str, expect: Option<u64>) -> code_sql::Result<Save> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let current = load_on(c, kind, owner_id)?;
        let revision = current.as_ref().map_or(0, |s| s.revision);
        if expect.is_some_and(|e| e != revision) {
            return Ok(Save::Conflict(current));
        }
        let cursor = take_cursors(c, owner_id, 1)?;
        let next = Saved { doc: doc.to_string(), revision: revision + 1, cursor, updated_at: now };
        c.execute(
            &format!(
                "INSERT INTO {} (owner_id, doc, revision, cursor, updated_at) VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT (owner_id) DO UPDATE SET doc = excluded.doc, revision = excluded.revision, cursor = excluded.cursor, updated_at = excluded.updated_at",
                kind.table()
            ),
            params![owner_id, next.doc, next.revision as i64, next.cursor as i64, next.updated_at],
        )?;
        Ok(Save::Saved(next))
    })
}

// ── routes ──

/// `{ <field>, revision, updated_at }`: what both routes answer (and a 409 with its `error`).
fn body(kind: Doc, saved: Option<&Saved>) -> Value {
    let mut v = match saved {
        Some(s) => json!({ "revision": s.revision, "updated_at": s.updated_at }),
        None => json!({ "revision": 0, "updated_at": null }),
    };
    v[kind.field()] = saved.map_or(Value::Null, |s| serde_json::from_str::<Value>(&s.doc).unwrap_or(Value::Null));
    v
}

async fn get(code: Arc<Code>, caller: Caller, owner: String, kind: Doc) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    Ok(Json(body(kind, load(code.db.sql(), kind, &o.id)?.as_ref())))
}

/// `GET …/canvas`: the owner's tokens, agents included (it's how the canvas looks); 404 to anyone
/// else, like the rest of the owner's agents.
pub(super) async fn get_canvas(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    get(code, caller, owner, Doc::Canvas).await
}

/// `GET …/settings`: the same readers as the canvas (an agent may follow its owner's
/// preferences); 404 to anyone else.
pub(super) async fn get_settings(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    get(code, caller, owner, Doc::Settings).await
}

/// Saving a document: the owner, not its agents (an agent's token works the threads; how the
/// owner lays them out and what they prefer is the owner's).
fn writable(code: &Code, caller: &Caller, owner: &str, kind: Doc) -> Result<crate::model::Owner> {
    let o = code.owner_or_404(owner)?;
    let (verb, what) = (kind.verb(), kind.what(owner));
    match caller {
        _ if Code::manages(caller, &o.id) => Ok(o),
        Caller::Anonymous => Err(Error::Unauthorized),
        c if c.agent().is_some() => Err(Error::Forbidden(format!("agent tokens can't {verb} {what}"))),
        _ => Err(Error::Forbidden(format!("not allowed to {verb} {what}"))),
    }
}

fn too_large(kind: Doc) -> Response {
    let msg = format!("the {} can be at most {} bytes of JSON", kind.field(), kind.max());
    (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({ "error": msg }))).into_response()
}

/// The request body, or `None` past `max` bytes (read no further than that).
async fn read_bounded(req: Request, max: usize) -> Result<Option<Vec<u8>>> {
    let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > max as u64) {
        return Ok(None);
    }
    let mut body = req.into_body().into_data_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::Invalid(format!("couldn't read the body: {e}")))?;
        if buf.len() + chunk.len() > max {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Some(buf))
}

/// `PUT …/{canvas,settings} {<field>: {…}, revision?}`: save it. With `revision`, only over that
/// revision (0: none saved yet), else a 409 with what is there; without, the last writer wins.
async fn put(code: Arc<Code>, caller: Caller, owner: String, req: Request, kind: Doc) -> Result<Response> {
    let o = writable(&code, &caller, &owner, kind)?;
    let field = kind.field();
    let Some(bytes) = read_bounded(req, kind.max_body()).await? else { return Ok(too_large(kind)) };
    let b: Value = serde_json::from_slice(&bytes).map_err(|e| Error::Invalid(format!("invalid JSON: {e}")))?;
    let Value::Object(mut b) = b else { return Err(Error::Invalid(format!("the body must be an object: {{\"{field}\": {{…}}, \"revision\"?: n}}"))) };
    let doc = match b.remove(field) {
        Some(doc @ Value::Object(_)) => doc,
        None => return Err(Error::Invalid(format!("{field} is required"))),
        Some(_) => return Err(Error::Invalid(format!("{field} must be a JSON object"))),
    };
    let expect = match b.get("revision") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| Error::Invalid("revision must be a whole number".into()))?),
    };
    let doc = serde_json::to_string(&doc).map_err(|e| Error::Internal(e.into()))?;
    if doc.len() > kind.max() {
        return Ok(too_large(kind));
    }
    // Written and published under the owner's order, so streams see cursors ascending.
    let hub = code.agents.hub(&o.id);
    let _order = hub.order.lock().await;
    match save(code.db.sql(), kind, &o.id, &doc, expect)? {
        Save::Saved(s) => {
            let _ = hub.events.send(Arc::new(s.event(kind)));
            Ok(Json(body(kind, Some(&s))).into_response())
        }
        Save::Conflict(current) => {
            let mut v = body(kind, current.as_ref());
            let have = current.as_ref().map_or(0, |s| s.revision);
            v["error"] = json!(format!("the {field} changed (it is at revision {have}); load it and save again"));
            Ok((StatusCode::CONFLICT, Json(v)).into_response())
        }
    }
}

pub(super) async fn put_canvas(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>, req: Request) -> Result<Response> {
    put(code, caller, owner, req, Doc::Canvas).await
}

pub(super) async fn put_settings(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>, req: Request) -> Result<Response> {
    put(code, caller, owner, req, Doc::Settings).await
}
