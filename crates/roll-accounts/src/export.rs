//! A person's own data, exported (docs/accounts.md, "Your data"), as GitHub's account data export
//! and GDPR's right to portability: one JSON document of everything roll keeps about them, never a
//! secret or a hash. Sudo mode: a stolen session cookie can't take it.

use crate::{Accounts, Result};
use code_sql::params;
use serde_json::{Value, json};

/// The most security-log lines an export carries (the newest).
const LOG_LINES: usize = 1000;

impl Accounts {
    /// Everything about `user` (their session `session` marked current): their account and sign-in
    /// methods, sessions, passkeys (never a key), the places they signed in from (when, not where:
    /// only a hash is kept), second-factor resets they asked for, their security log, and their
    /// roll tokens (names, rules, dates; never a secret).
    pub fn export(&self, user: &str, session: &str) -> Result<Value> {
        let account = self.user(user)?;
        let sessions = self.sessions_of(user, session)?;
        let passkeys = self.passkeys(user)?;
        let places: Vec<Value> = self.db.read(|c| {
            c.query("SELECT first_at, last_at FROM sign_in_places WHERE user_id = ? ORDER BY last_at DESC", params![user], |r| {
                Ok(json!({ "first_at": r.get::<_, i64>(0)?, "last_at": r.get::<_, i64>(1)? }))
            })
        })?;
        let resets: Vec<Value> = self.reset_requests()?.into_iter().filter(|r| r.user == user).map(|r| json!(r)).collect();
        let (log, tokens) = match self.audit_hook() {
            Some(h) => (h.activity(user, None, LOG_LINES).into_iter().map(|a| json!(a)).collect::<Vec<_>>(), h.tokens(user)),
            None => (Vec::new(), Vec::new()),
        };
        Ok(json!({
            "exported_at": self.db.now_ms()?,
            "about": "Everything roll keeps about your account. Secrets (session tokens, codes, keys, token secrets) are never in it.",
            "account": account,
            "sessions": sessions,
            "passkeys": passkeys,
            "sign_in_places": places,
            "second_factor_resets": resets,
            "security_log": log,
            "tokens": tokens,
        }))
    }
}
