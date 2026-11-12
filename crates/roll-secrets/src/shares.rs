//! One-time sharing (docs/secrets.md, "Sharing a value once"): a value handed to someone outside
//! roll through a link that opens a few times (once by default) before it expires.
//!
//! The server never sees the value. The client encrypts it with a random key (AES-256-GCM,
//! `roll-share-v1` as associated data) and keeps the key in the link's `#fragment`, which
//! browsers never send. The server keeps the ciphertext, counts the opens, and drops the ciphertext
//! at the last one or at expiry. Opening is a `POST`, so a link unfurler (a chat's preview) can't
//! use it up.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, audit, names};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS shares (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    note TEXT NOT NULL,
    ciphertext BLOB,
    views_left INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    opened_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS shares_project ON shares (project_id, created_at);
CREATE INDEX IF NOT EXISTS shares_expiry ON shares (expires_at);
";

/// The largest ciphertext (a 64 KiB value, its nonce and tag, in base64url).
const CIPHERTEXT_MAX: usize = 90 * 1024;
/// Shares a project may have open at once.
const OPEN_MAX: i64 = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewShare {
    /// base64url(nonce ‖ AES-256-GCM ciphertext), made by the client.
    pub ciphertext: String,
    /// Shown to whoever opens it ("the staging database password"); never the value.
    #[serde(default)]
    pub note: String,
    /// 1–10 opens (default 1).
    pub views: Option<i64>,
    /// 5 minutes to 7 days (default 24 hours).
    pub ttl_minutes: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Share {
    pub id: String,
    pub note: String,
    pub views_left: i64,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub opened_at: Option<i64>,
    /// Whether it can still be opened.
    pub open: bool,
    /// `<dashboard>/share/<id>` when the server knows the dashboard's address (`ROLL_WEB_URL`);
    /// the client appends `#<key>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// What a share's page shows before it's opened (public).
#[derive(Debug, Serialize)]
pub struct Peek {
    pub note: String,
    pub views_left: i64,
    pub expires_at: i64,
}

impl Secrets {
    pub fn create_share(&self, project: &str, n: NewShare, caller: &Caller) -> Result<Share> {
        caller.need(Action::Read, &Scope::project(project))?;
        let raw = URL_SAFE_NO_PAD.decode(n.ciphertext.trim()).map_err(|_| Error::BadRequest("ciphertext is base64url".into()))?;
        if raw.len() < 12 + 16 || n.ciphertext.len() > CIPHERTEXT_MAX {
            return Err(Error::BadRequest(format!("ciphertext is a nonce, a value of at most {} KiB and a tag", names::VALUE_MAX / 1024)));
        }
        names::comment(&n.note)?;
        let views = n.views.unwrap_or(1);
        let ttl = n.ttl_minutes.unwrap_or(24 * 60);
        if !(1..=10).contains(&views) || !(5..=7 * 24 * 60).contains(&ttl) {
            return Err(Error::BadRequest("views is 1–10 and ttl_minutes 5–10080".into()));
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        let id = names::new_id("shr", 24);
        self.db.write(|c| {
            let open: i64 = c.query_row("SELECT COUNT(*) FROM shares WHERE project_id = ? AND ciphertext IS NOT NULL AND expires_at > ?", params![project, now], |r| r.get(0))?;
            if open >= OPEN_MAX {
                return Err(Error::Conflict("conflict", format!("{OPEN_MAX} shares are open: revoke some first")));
            }
            c.execute(
                "INSERT INTO shares (id, project_id, note, ciphertext, views_left, created_by, created_at, expires_at, opened_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)",
                params![id, project, n.note, raw, views, caller.principal, now, now + ttl * 60_000],
            )?;
            audit::record(c, project, caller, "share.create", None, None, &[], &format!("shared a value ({views} open{}, {ttl} min): {}", if views == 1 { "" } else { "s" }, n.note), now)?;
            let link = self.web_url.as_ref().map(|w| format!("{}/share/{id}", w.trim_end_matches('/')));
            Ok(Share { id: id.clone(), note: n.note.clone(), views_left: views, created_by: caller.principal.clone(), created_at: now, expires_at: now + ttl * 60_000, opened_at: None, open: true, link })
        })
    }

    pub fn shares(&self, project: &str, caller: &Caller) -> Result<Vec<Share>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.read(|c| {
            Ok(c.query(
                "SELECT id, note, views_left, created_by, created_at, expires_at, opened_at, ciphertext IS NOT NULL FROM shares WHERE project_id = ? ORDER BY created_at DESC LIMIT 200",
                [project],
                |r| {
                    let expires_at: i64 = r.get(5)?;
                    Ok(Share {
                        id: r.get(0)?,
                        note: r.get(1)?,
                        views_left: r.get(2)?,
                        created_by: r.get(3)?,
                        created_at: r.get(4)?,
                        expires_at,
                        opened_at: r.get(6)?,
                        open: r.get::<_, i64>(7)? != 0 && expires_at > now,
                        link: None,
                    })
                },
            )?)
        })
    }

    /// Revoke a share: its ciphertext goes now.
    pub fn revoke_share(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        // Its creator's, or an admin's: another reader of the project can't take a link away.
        let admin = caller.allows(Action::Admin, &Scope::project(project));
        self.db.write(|c| {
            let by: Option<String> = c.query_row("SELECT created_by FROM shares WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?;
            let by = by.ok_or_else(|| Error::NotFound(format!("no share {id}")))?;
            if by != caller.principal && !admin {
                return Err(Error::Forbidden("only whoever made a share, or an admin, revokes it".into()));
            }
            c.execute("UPDATE shares SET ciphertext = NULL, views_left = 0 WHERE id = ? AND project_id = ?", [id, project])?;
            audit::record(c, project, caller, "share.revoke", None, None, &[], &format!("revoked share {}", &id[..12.min(id.len())]), now)
        })
    }

    /// What a share's page shows before opening it (public; uses up nothing).
    pub fn peek_share(&self, id: &str) -> Result<Peek> {
        let now = self.db.now_ms()?;
        let row = self.db.read(|c| {
            c.query_row("SELECT note, views_left, expires_at FROM shares WHERE id = ? AND ciphertext IS NOT NULL AND expires_at > ?", params![id, now], |r| {
                Ok(Peek { note: r.get(0)?, views_left: r.get(1)?, expires_at: r.get(2)? })
            })
            .optional()
        })?;
        row.ok_or_else(gone)
    }

    /// Open a share (public): its ciphertext, one open used up; the last one drops it.
    pub fn open_share(&self, id: &str, ip: Option<String>) -> Result<String> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let row: Option<(String, Vec<u8>, i64)> = c
                .query_row("SELECT project_id, ciphertext, views_left FROM shares WHERE id = ? AND ciphertext IS NOT NULL AND expires_at > ?", params![id, now], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let (project, ct, left) = row.ok_or_else(gone)?;
            if left <= 1 {
                c.execute("UPDATE shares SET ciphertext = NULL, views_left = 0, opened_at = ? WHERE id = ?", params![now, id])?;
            } else {
                c.execute("UPDATE shares SET views_left = views_left - 1, opened_at = ? WHERE id = ?", params![now, id])?;
            }
            let opener = Caller { principal: "share-link".into(), grants: std::sync::Arc::new(crate::access::Everything), ip: ip.clone() };
            audit::record(c, &project, &opener, "share.open", None, None, &[], &format!("share {} opened ({} left)", &id[..12.min(id.len())], (left - 1).max(0)), now)?;
            Ok(URL_SAFE_NO_PAD.encode(ct))
        })
    }

    /// Drop the ciphertexts of expired shares (the sweeper).
    pub(crate) fn expire_shares(&self) -> Result<usize> {
        let now = self.db.now_ms()?;
        Ok(self.db.write(|c| c.execute("UPDATE shares SET ciphertext = NULL WHERE ciphertext IS NOT NULL AND expires_at <= ?", [now]))?)
    }
}

fn gone() -> Error {
    Error::NotFound("this link was already opened, expired or was revoked".into())
}
