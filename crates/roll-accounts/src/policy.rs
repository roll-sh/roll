//! A second factor for everyone (docs/accounts.md, "Second factor for everyone"), as GitHub's
//! organizations and Google Workspace require it: while it's on, a person whose session has no
//! second factor (no authenticator app, no passkey) reaches only their account, where they set one
//! up; rolld's gate asks on every other request (`roll_auth::Sessions::missing`).
//! A session their company's SSO provider signed in is the provider's to check, so it passes.
//!
//! Phishing-resistant sign-in (docs/accounts.md, as Okta's authenticator policy and Google's
//! Advanced Protection ask it): for admins or everyone, a session must have used a passkey (signed
//! in with one, finished its second factor with one, or confirmed it's you with one), because an
//! email code or an authenticator app's code can be relayed by a lookalike page and a passkey
//! can't. Until then it reaches only the person's account, where they add a passkey and use it.
//! SSO sessions pass: the provider's own policy decides how they sign in.

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
    /// Whose sessions must use a passkey: admins, or everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phishing_resistant: Option<Whom>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Whom {
    Admins,
    Everyone,
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

    /// The policy as `by` sets it (`None`: the operator's token). An admin turning on
    /// phishing-resistant sign-in must be using a passkey themselves, or they'd lock themselves
    /// out of everything but their account page.
    pub fn set_account_policy_by(&self, p: AccountPolicy, by: Option<(&str, &str)>) -> Result<AccountPolicy> {
        if let (Some(_), Some((user, session))) = (p.phishing_resistant, by)
            && !self.session_used_passkey(user, session)?
        {
            return Err(crate::Error::Conflict("confirm it's you with a passkey first (add one on your account page): with this on, a session that hasn't used one reaches only its account".into()));
        }
        self.set_account_policy(p)
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

    /// Whether `user`'s session `session` used a passkey (or came from their company's SSO).
    fn session_used_passkey(&self, user: &str, session: &str) -> Result<bool> {
        let row = self.db.read(|c| c.query_row("SELECT method, passkey_at FROM sessions WHERE id = ? AND user_id = ?", params![session, user], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?))).optional())?;
        Ok(row.is_some_and(|(method, at)| method.as_deref() == Some("sso") || at.is_some()))
    }

    /// A passkey was used in session `session`: it counts as phishing-resistant from now on.
    pub(crate) fn mark_passkey(&self, session: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| c.execute("UPDATE sessions SET passkey_at = ? WHERE id = ?", params![now, session]))?;
        Ok(())
    }

    /// What `user`'s session `session` lacks that the policy requires: a passkey used in it, then
    /// a second factor. SSO sessions are their provider's to check.
    pub(crate) fn missing(&self, user: &str, session: &str) -> Result<Option<roll_auth::Missing>> {
        let p = self.account_policy()?;
        if !p.require_second_factor && p.phishing_resistant.is_none() {
            return Ok(None);
        }
        let row = self.db.read(|c| {
            c.query_row("SELECT s.method, s.passkey_at, u.admin FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.id = ? AND s.user_id = ?", params![session, user], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, bool>(2)?))
            })
            .optional()
        })?;
        let Some((method, passkey_at, admin)) = row else { return Ok(None) };
        if method.as_deref() == Some("sso") {
            return Ok(None);
        }
        let covered = match p.phishing_resistant {
            Some(Whom::Everyone) => true,
            Some(Whom::Admins) => admin,
            None => false,
        };
        if covered && passkey_at.is_none() {
            return Ok(Some(roll_auth::Missing::Passkey));
        }
        if p.require_second_factor && !self.has_second_factor(user)? {
            return Ok(Some(roll_auth::Missing::SecondFactor));
        }
        Ok(None)
    }
}
