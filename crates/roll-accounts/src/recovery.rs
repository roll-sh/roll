//! Second-factor resets an admin decides (docs/accounts.md, "Recovery"), as Okta's and Google
//! Workspace's admins reset a person's factors: someone who lost their authenticator app and their
//! recovery codes signs in with their first factor (an email code, GitHub, Google), asks with a
//! reason, and an admin, after checking it's them some other way (a call, in person), approves:
//! their TOTP and recovery codes go, they sign in again and set it up again. Admins are mailed; the
//! person is mailed either way; each step is in their security log. An admin never approves their
//! own; approving or denying is sudo mode.

use crate::audit::{AccountEvent, Who};
use crate::{Accounts, Error, Mail, Result};
use code_sql::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

/// A reset someone asked for.
#[derive(Clone, Debug, Serialize)]
pub struct ResetRequest {
    pub id: String,
    pub user: String,
    pub email: String,
    pub handle: String,
    pub reason: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: i64,
    /// `pending`, `approved` or `denied`.
    pub state: String,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
}

const COLUMNS: &str = "r.id, r.user_id, u.email, u.handle, r.reason, r.ip, r.user_agent, r.created_at, r.state, r.decided_by, r.decided_at";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<ResetRequest> {
    Ok(ResetRequest {
        id: r.get(0)?,
        user: r.get(1)?,
        email: r.get(2)?,
        handle: r.get(3)?,
        reason: r.get(4)?,
        ip: r.get(5)?,
        user_agent: r.get(6)?,
        created_at: r.get(7)?,
        state: r.get(8)?,
        decided_by: r.get(9)?,
        decided_at: r.get(10)?,
    })
}

impl Accounts {
    fn reset_request(&self, id: &str) -> Result<ResetRequest> {
        self.db
            .read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM reset_requests r JOIN users u ON u.id = r.user_id WHERE r.id = ?"), [id], row).optional())?
            .ok_or_else(|| Error::NotFound(format!("no reset request {id}")))
    }

    /// `user` (signed in up to their second factor) asks an admin to reset it. Their one pending
    /// request is answered again rather than a second made; three a day at most.
    pub(crate) fn request_reset(self: &Arc<Self>, user: &str, reason: &str, who: &Who) -> Result<ResetRequest> {
        let reason: String = reason.chars().filter(|c| !c.is_control() || *c == '\n').collect::<String>().trim().to_string();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::BadRequest("say why, in 1 to 500 characters: an admin reads it before calling you".into(), None));
        }
        let pending = self.db.read(|c| c.query_row("SELECT id FROM reset_requests WHERE user_id = ? AND state = 'pending'", [user], |r| r.get::<_, String>(0)).optional())?;
        if let Some(id) = pending {
            return self.reset_request(&id);
        }
        if !self.attempt(&format!("reset:{user}"), 3, 24 * 60 * 60 * 1000)? {
            return Err(Error::Unavailable("three requests a day at most: an admin has them".into(), "too_many_requests"));
        }
        let id = crate::new_id("rst");
        let now = self.db.now_ms()?;
        let (ip, ua) = (who.ip.map(|i| i.to_string()), who.user_agent.clone());
        self.db.write(|c| {
            c.execute(
                "INSERT INTO reset_requests (id, user_id, reason, ip, user_agent, created_at, state) VALUES (?, ?, ?, ?, ?, ?, 'pending')",
                params![id, user, reason, ip, ua, now],
            )?;
            Ok::<_, Error>(())
        })?;
        let req = self.reset_request(&id)?;
        self.record(AccountEvent { actor: user.into(), action: "recovery.request".into(), method: "-".into(), target: Some(user.into()), ok: true, status: 202, ip: who.ip, user_agent: who.user_agent.clone(), detail: json!({ "request": id }) });
        // Every admin but the person: someone else checks it's them.
        let admins: Vec<String> = self.db.read(|c| c.query("SELECT email FROM users WHERE admin = 1 AND disabled = 0 AND id <> ? ORDER BY created_at LIMIT 50", [user], |r| r.get::<_, String>(0)))?;
        for to in admins {
            let text = format!(
                "{} ({}) can't give their second factor and asks you to reset it:\n\n{}\n\nCheck it's them some other way first (a call, in person): their email code alone got them this far. Then approve or deny it at {}/dashboard/account#recovery.\n",
                req.handle, req.email, req.reason, self.config.web_url
            );
            self.send_later(Mail { to, subject: format!("{} asks for a second-factor reset", req.handle), text }, "reset request");
        }
        Ok(req)
    }

    /// Requests, pending first, then the latest decided (50 at most).
    pub fn reset_requests(&self) -> Result<Vec<ResetRequest>> {
        Ok(self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM reset_requests r JOIN users u ON u.id = r.user_id ORDER BY r.state = 'pending' DESC, r.created_at DESC LIMIT 50"), (), row))?)
    }

    /// An admin (`by`: their id, or `operator`) approves or denies request `id`. Approving takes the
    /// person's TOTP and recovery codes away and ends their sign-ins waiting for it.
    pub(crate) fn decide_reset(self: &Arc<Self>, id: &str, approve: bool, by: &str, who: &Who) -> Result<ResetRequest> {
        let req = self.reset_request(id)?;
        if req.user == by {
            return Err(Error::Forbidden("another admin decides your own reset".into(), "own_request"));
        }
        let now = self.db.now_ms()?;
        let state = if approve { "approved" } else { "denied" };
        self.db.write(|c| {
            if c.execute("UPDATE reset_requests SET state = ?, decided_by = ?, decided_at = ? WHERE id = ? AND state = 'pending'", params![state, by, now, id])? == 0 {
                return Err(Error::Conflict("that request was decided already".into()));
            }
            if approve {
                c.execute("DELETE FROM totp WHERE user_id = ?", [req.user.as_str()])?;
                c.execute("DELETE FROM recovery_codes WHERE user_id = ?", [req.user.as_str()])?;
                c.execute("DELETE FROM sessions WHERE user_id = ? AND level = 'mfa'", [req.user.as_str()])?;
            }
            Ok(())
        })?;
        self.record(AccountEvent {
            actor: by.into(),
            action: format!("recovery.{}", if approve { "approve" } else { "deny" }),
            method: "-".into(),
            target: Some(req.user.clone()),
            ok: true,
            status: 200,
            ip: who.ip,
            user_agent: who.user_agent.clone(),
            detail: json!({ "request": id }),
        });
        let text = if approve {
            format!("An admin reset your roll account's second factor, as you asked. Sign in with your email code and set up an authenticator app or a passkey again: {}/dashboard/account\n\nIf you didn't ask for this, tell an admin now.\n", self.config.web_url)
        } else {
            "An admin declined to reset your roll account's second factor. Ask them what they need to check it's you.\n".to_string()
        };
        self.send_later(Mail { to: req.email.clone(), subject: format!("Your second-factor reset was {state}"), text }, "reset decided");
        self.reset_request(id)
    }
}
