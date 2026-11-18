//! A second factor for everyone (docs/accounts.md, "Second factor for everyone"), as GitHub's
//! organizations and Google Workspace require it: while it's on, a person whose session has no
//! second factor (no authenticator app, no passkey) reaches only their account, where they set one
//! up; rolld's gate asks on every other request (`roll_auth::Sessions::second_factor_missing`).
//! A session their company's SSO provider signed in is the provider's to check, so it passes.

use crate::{Accounts, Result};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AccountPolicy {
    /// Everyone needs a second factor (an authenticator app or a passkey).
    #[serde(default)]
    pub require_second_factor: bool,
    /// A session unused this long ends (Okta's idle session lifetime), in minutes: 15 to a week;
    /// none is roll's week.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_minutes: Option<u32>,
}

/// The shortest idle timeout: a session's last use is noted every five minutes.
const MIN_IDLE: u32 = 15;
const MAX_IDLE: u32 = 7 * 24 * 60;

impl AccountPolicy {
    fn checked(self) -> Result<Self> {
        if self.idle_minutes.is_some_and(|m| !(MIN_IDLE..=MAX_IDLE).contains(&m)) {
            return Err(crate::Error::BadRequest(format!("an idle timeout is {MIN_IDLE} minutes to a week ({MAX_IDLE} minutes)"), Some("policy")));
        }
        Ok(self)
    }

    /// How long a session may go unused, in ms.
    pub(crate) fn idle_ms(&self) -> i64 {
        self.idle_minutes.map_or(crate::SESSION_IDLE_MS, |m| (m as i64 * 60_000).min(crate::SESSION_IDLE_MS))
    }
}

impl Accounts {
    pub fn account_policy(&self) -> Result<AccountPolicy> {
        let raw = self.db.read(|c| c.query_row("SELECT policy FROM account_policy WHERE id = 1", (), |r| r.get::<_, String>(0)).optional())?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    pub fn set_account_policy(&self, p: AccountPolicy) -> Result<AccountPolicy> {
        let p = p.checked()?;
        let s = serde_json::to_string(&p).map_err(|e| crate::Error::Internal(e.to_string()))?;
        self.db.write(|c| c.execute("INSERT INTO account_policy (id, policy) VALUES (1, ?) ON CONFLICT (id) DO UPDATE SET policy = excluded.policy", [s.as_str()]))?;
        Ok(p)
    }

    /// Whether `user` has a second factor: an authenticator app on, or a passkey.
    pub(crate) fn has_second_factor(&self, user: &str) -> Result<bool> {
        Ok(self.db.read(|c| {
            let totp = c.query_row("SELECT 1 FROM totp WHERE user_id = ? AND enabled = 1", [user], |_| Ok(())).optional()?.is_some();
            Ok::<_, code_sql::Error>(totp || c.query_row("SELECT 1 FROM passkeys WHERE user_id = ? LIMIT 1", [user], |_| Ok(())).optional()?.is_some())
        })?)
    }

    /// Whether `user`'s session `session` lacks the second factor the policy requires.
    pub(crate) fn second_factor_missing(&self, user: &str, session: &str) -> Result<bool> {
        if !self.account_policy()?.require_second_factor {
            return Ok(false);
        }
        let sso = self.db.read(|c| c.query_row("SELECT method FROM sessions WHERE id = ?", params![session], |r| r.get::<_, Option<String>>(0)).optional())?.flatten().as_deref() == Some("sso");
        Ok(!sso && !self.has_second_factor(user)?)
    }
}
