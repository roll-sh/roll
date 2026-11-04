//! `/v1/code/agents/{owner}/canvas`: the canvas as its owner arranged it, kept by roll so every
//! device (the dashboard on a laptop or phone, the desktop app) shows the same (docs/code/agents.md,
//! "The canvas"). The document is the dashboard's and opaque here: one JSON object of at most
//! [`MAX_DOC`] bytes, with a revision that counts its saves. A save takes an owner cursor in the
//! same transaction and goes out on the owner's event stream as `canvas`, so open devices load it
//! again and a reconnecting stream catches up on it from the table.

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
"#;

/// The largest document, as compact JSON.
pub const MAX_DOC: usize = 64 << 10;
/// The largest request body: the document and the `{"canvas": …, "revision": …}` around it,
/// with room for whitespace the client may add.
const MAX_BODY: usize = MAX_DOC + (4 << 10);

/// The canvas as stored.
#[derive(Clone, Debug)]
pub struct Saved {
    pub doc: String,
    pub revision: u64,
    pub cursor: u64,
    pub updated_at: i64,
}

impl Saved {
    pub(crate) fn event(&self) -> StreamEvent {
        StreamEvent::Canvas { cursor: self.cursor, revision: self.revision, updated_at: self.updated_at.max(0) as u64 }
    }
}

fn saved_row(r: &Row<'_>) -> code_sql::Result<Saved> {
    Ok(Saved { doc: r.get(0)?, revision: r.get::<_, i64>(1)?.max(0) as u64, cursor: r.get::<_, i64>(2)?.max(0) as u64, updated_at: r.get(3)? })
}

const COLS: &str = "doc, revision, cursor, updated_at";

pub fn load(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Option<Saved>> {
    db.query_row(&format!("SELECT {COLS} FROM agent_canvas WHERE owner_id = ?"), [owner_id], saved_row).optional()
}

fn load_on(c: &Conn<'_>, owner_id: &str) -> code_sql::Result<Option<Saved>> {
    c.query_row(&format!("SELECT {COLS} FROM agent_canvas WHERE owner_id = ?"), [owner_id], saved_row).optional()
}

/// The canvas if it was saved after `cursor` (an event stream catching up).
pub fn after(db: &code_sql::Db, owner_id: &str, cursor: u64) -> code_sql::Result<Option<Saved>> {
    db.query_row(&format!("SELECT {COLS} FROM agent_canvas WHERE owner_id = ? AND cursor > ?"), params![owner_id, cursor as i64], saved_row).optional()
}

pub enum Save {
    Saved(Saved),
    /// `expect` wasn't the revision: what is there now (`None`: nothing yet).
    Conflict(Option<Saved>),
}

/// Save `doc` as the owner's canvas. With `expect`, only over that revision (0: none saved yet);
/// without, over whatever is there.
pub fn save(db: &code_sql::Db, owner_id: &str, doc: &str, expect: Option<u64>) -> code_sql::Result<Save> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let current = load_on(c, owner_id)?;
        let revision = current.as_ref().map_or(0, |s| s.revision);
        if expect.is_some_and(|e| e != revision) {
            return Ok(Save::Conflict(current));
        }
        let cursor = take_cursors(c, owner_id, 1)?;
        let next = Saved { doc: doc.to_string(), revision: revision + 1, cursor, updated_at: now };
        c.execute(
            "INSERT INTO agent_canvas (owner_id, doc, revision, cursor, updated_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (owner_id) DO UPDATE SET doc = excluded.doc, revision = excluded.revision, cursor = excluded.cursor, updated_at = excluded.updated_at",
            params![owner_id, next.doc, next.revision as i64, next.cursor as i64, next.updated_at],
        )?;
        Ok(Save::Saved(next))
    })
}

// ── routes ──

/// `{ canvas, revision, updated_at }`: what both routes answer (and a 409 with its `error`).
fn body(saved: Option<&Saved>) -> Value {
    match saved {
        Some(s) => {
            let doc = serde_json::from_str::<Value>(&s.doc).unwrap_or(Value::Null);
            json!({ "canvas": doc, "revision": s.revision, "updated_at": s.updated_at })
        }
        None => json!({ "canvas": null, "revision": 0, "updated_at": null }),
    }
}

/// `GET …/canvas`: the owner's tokens, agents included (it's how the canvas looks); 404 to anyone
/// else, like the rest of the owner's agents.
pub(super) async fn get(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>) -> Result<Json<Value>> {
    let o = readable(&code, &caller, &owner)?;
    Ok(Json(body(load(code.db.sql(), &o.id)?.as_ref())))
}

/// Arranging the canvas: the owner, not its agents (an agent's token works the threads; how the
/// owner lays them out is the owner's).
fn arrangeable(code: &Code, caller: &Caller, owner: &str) -> Result<crate::model::Owner> {
    let o = code.owner_or_404(owner)?;
    match caller {
        _ if Code::manages(caller, &o.id) => Ok(o),
        Caller::Anonymous => Err(Error::Unauthorized),
        c if c.agent().is_some() => Err(Error::Forbidden(format!("agent tokens can't arrange {owner}'s canvas"))),
        _ => Err(Error::Forbidden(format!("not allowed to arrange {owner}'s canvas"))),
    }
}

fn too_large() -> Response {
    let msg = format!("the canvas can be at most {MAX_DOC} bytes of JSON");
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

/// `PUT …/canvas {canvas: {…}, revision?}`: save it. With `revision`, only over that revision
/// (0: none saved yet), else a 409 with what is there; without, the last writer wins.
pub(super) async fn put(State(code): State<Arc<Code>>, caller: Caller, Path(owner): Path<String>, req: Request) -> Result<Response> {
    let o = arrangeable(&code, &caller, &owner)?;
    let Some(bytes) = read_bounded(req, MAX_BODY).await? else { return Ok(too_large()) };
    let b: Value = serde_json::from_slice(&bytes).map_err(|e| Error::Invalid(format!("invalid JSON: {e}")))?;
    let Value::Object(mut b) = b else { return Err(Error::Invalid("the body must be an object: {\"canvas\": {…}, \"revision\"?: n}".into())) };
    let doc = match b.remove("canvas") {
        Some(doc @ Value::Object(_)) => doc,
        None => return Err(Error::Invalid("canvas is required".into())),
        Some(_) => return Err(Error::Invalid("canvas must be a JSON object".into())),
    };
    let expect = match b.get("revision") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| Error::Invalid("revision must be a whole number".into()))?),
    };
    let doc = serde_json::to_string(&doc).map_err(|e| Error::Internal(e.into()))?;
    if doc.len() > MAX_DOC {
        return Ok(too_large());
    }
    // Written and published under the owner's order, so streams see cursors ascending.
    let hub = code.agents.hub(&o.id);
    let _order = hub.order.lock().await;
    match save(code.db.sql(), &o.id, &doc, expect)? {
        Save::Saved(s) => {
            let _ = hub.events.send(Arc::new(s.event()));
            Ok(Json(body(Some(&s))).into_response())
        }
        Save::Conflict(current) => {
            let mut v = body(current.as_ref());
            let have = current.as_ref().map_or(0, |s| s.revision);
            v["error"] = json!(format!("the canvas changed (it is at revision {have}); load it and save again"));
            Ok((StatusCode::CONFLICT, Json(v)).into_response())
        }
    }
}
