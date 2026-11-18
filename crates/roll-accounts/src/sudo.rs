//! Sudo mode (GitHub's, docs/accounts.md "Sudo mode"): what adds or removes a way into an account,
//! or changes who is an admin or how a company signs in, needs the person to have proved who they
//! are in the last two hours: a sign-in, or a confirmation with their authenticator app, a
//! passkey or an email code. A stolen session cookie alone can't add its own passkey, unlink the
//! owner's GitHub, or make someone an admin. The operator's token has no session: it's exempt.

use crate::{Accounts, Error, Result};
use code_sql::{OptionalExtension, params};

/// How long a proof lasts.
pub(crate) const SUDO_MS: i64 = 2 * 60 * 60 * 1000;

impl Accounts {
    /// A shorter window (tests).
    pub fn set_sudo_window(&self, ms: i64) {
        self.sudo_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    fn sudo_ms(&self) -> i64 {
        self.sudo_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Until when session `session`'s proof lasts (its sign-in, or its last confirmation).
    pub(crate) fn fresh_until(&self, session: &str) -> Result<i64> {
        let at = self.db.read(|c| c.query_row("SELECT COALESCE(verified_at, created_at) FROM sessions WHERE id = ?", [session], |r| r.get::<_, i64>(0)).optional())?;
        Ok(at.map(|t| t + self.sudo_ms()).unwrap_or(0))
    }

    /// Refused unless session `session` proved who it is recently.
    pub(crate) fn fresh(&self, session: &str) -> Result<()> {
        if self.fresh_until(session)? > self.db.now_ms()? {
            return Ok(());
        }
        Err(Error::Forbidden("confirm it's you first (GET /v1/accounts/me/reauth says how): this changes how your account is reached".into(), "reauth_required"))
    }

    /// The person proved who they are again: their session is fresh for another two hours.
    pub(crate) fn mark_verified(&self, user: &str, session: &str) -> Result<i64> {
        let now = self.db.now_ms()?;
        if self.db.write(|c| c.execute("UPDATE sessions SET verified_at = ? WHERE id = ? AND user_id = ? AND level = 'full'", params![now, session, user]))? == 0 {
            return Err(Error::signed_out());
        }
        Ok(now + self.sudo_ms())
    }

    /// How `user` can confirm it's them: their authenticator app (or a recovery code), a
    /// passkey, an email code.
    pub(crate) fn reauth_methods(&self, user: &str) -> Result<Vec<&'static str>> {
        let totp = self.db.read(|c| c.query_row("SELECT 1 FROM totp WHERE user_id = ? AND enabled = 1", [user], |_| Ok(())).optional())?.is_some();
        let passkey = self.webauthn_site().is_ok() && !self.passkeys(user)?.is_empty();
        let mut m = Vec::new();
        if totp {
            m.push("totp");
        }
        if passkey {
            m.push("passkey");
        }
        if self.mail_on() {
            m.push("email");
        }
        Ok(m)
    }
}
