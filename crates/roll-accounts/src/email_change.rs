//! A person moves their account to a new address (GitHub's, docs/accounts.md "Your address"): from
//! a fresh session (sudo mode), a code goes to the new address, and the change happens only once
//! it comes back; the old address is then told where the account went. Sign-in codes, invites'
//! matches and every mail roll sends follow `users.email`, so the change is that one column.
//! An address that already has an account gets a note instead of a code (the answer doesn't say
//! which), and an address someone's company manages stays with it: SCIM-provisioned people, and
//! domains whose SSO connection enforces its sign-in, on either side of the move.

use crate::{CODE_FAILS, CODE_TRIES, CODE_TTL_MS, Accounts, Error, MINUTE, Mail, Result, SENDS_PER_HOUR, User, normalize_email, six_digits, twofactor};
use code_sql::{OptionalExtension, params};

impl Accounts {
    /// Whether `user` may move from `old` to `new` at all.
    fn email_change_allowed(&self, user: &str, old: &str, new: &str) -> Result<()> {
        if self.db.read(|c| c.query_row("SELECT 1 FROM scim_users WHERE user_id = ? LIMIT 1", [user], |_| Ok(())).optional())?.is_some() {
            return Err(Error::Forbidden("your company's provider (SCIM) manages your address: change it there".into(), "managed"));
        }
        if let Some(name) = self.sso_required_for(old)? {
            return Err(Error::Forbidden(format!("{name} manages your address: your organization requires its sign-in"), "sso_required"));
        }
        if let Some(name) = self.sso_required_for(new)? {
            return Err(Error::Forbidden(format!("addresses at that domain sign in with {name}: an account can't move there"), "sso_required"));
        }
        Ok(())
    }

    /// Start moving `user` to `email` under `challenge`: the mail to send (a code, or a note to an
    /// address that already has an account; none when that address got too many this hour).
    pub fn email_change_start(&self, user: &str, challenge: &str, email: &str) -> Result<Option<Mail>> {
        let email = normalize_email(email)?;
        let me = self.user(user)?;
        if me.email == email {
            return Err(Error::BadRequest("that's already your address".into(), Some("email")));
        }
        self.email_change_allowed(user, &me.email, &email)?;
        if !self.attempt(&format!("change:{user}"), SENDS_PER_HOUR, 60 * MINUTE)? {
            return Err(Error::Forbidden("too many address changes started: try again in an hour".into(), "throttled"));
        }
        if !self.attempt(&format!("send:{email}"), SENDS_PER_HOUR, 60 * MINUTE)? {
            return Ok(None);
        }
        let now = self.db.now_ms()?;
        let taken = self.db.read(|c| c.query_row("SELECT 1 FROM users WHERE email = ?", [email.as_str()], |_| Ok(())).optional())?.is_some();
        if taken {
            // Its owner learns someone tried; the person asking learns nothing they can tell apart.
            self.db.write(|c| c.execute("DELETE FROM email_changes WHERE user_id = ? OR expires_at <= ?", params![user, now]))?;
            return Ok(Some(Mail {
                to: email,
                subject: "Someone tried to move a roll account to this address".into(),
                text: "Someone asked to make this address the one their roll account uses, but it already has a roll account (yours). Nothing changed, and no code was sent.\n\nIf it was you with a second account, ask your admin.\n".into(),
            }));
        }
        let code = zeroize::Zeroizing::new(six_digits());
        let hash = twofactor::hash_code(&code)?;
        self.db.write(|c| {
            // One change at a time: a new one replaces the last.
            c.execute("DELETE FROM email_changes WHERE user_id = ? OR expires_at <= ?", params![user, now])?;
            c.execute(
                "INSERT INTO email_changes (id, user_id, email, code_hash, attempts, created_at, expires_at) VALUES (?, ?, ?, ?, 0, ?, ?)",
                params![challenge, user, email, hash, now, now + CODE_TTL_MS],
            )
        })?;
        Ok(Some(Mail {
            to: email,
            subject: format!("{} is your code to move your roll account here", *code),
            text: format!(
                "Enter {} on your roll account page to make this the address {} signs in with. It works once, for the next 10 minutes.\n\nIf you didn't ask for it, ignore this mail: nothing changes.\n",
                *code, me.handle
            ),
        }))
    }

    /// The code from the new address: the account moves there, in one write. Returns the person
    /// and the mail for their old address.
    pub fn email_change_confirm(&self, user: &str, challenge: &str, code: &str) -> Result<(User, Mail)> {
        let now = self.db.now_ms()?;
        // As a sign-in code: a try is used up before the code is checked.
        let row = self.db.write(|c| {
            if c.execute("UPDATE email_changes SET attempts = attempts + 1 WHERE id = ? AND user_id = ? AND attempts < ? AND expires_at > ?", params![challenge, user, CODE_TRIES, now])? == 0 {
                return Ok(None);
            }
            c.query_row("SELECT email, code_hash FROM email_changes WHERE id = ?", [challenge], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()
        })?;
        let Some((email, hash)) = row else { return Err(Error::bad_code()) };
        let key = format!("change-code:{user}");
        if !self.attempt(&key, CODE_FAILS, 15 * MINUTE)? || !twofactor::verify_code(code.trim(), &hash) {
            return Err(Error::bad_code());
        }
        let old = self.user(user)?;
        // The rules may have changed in the ten minutes (a connection now enforces its domain).
        self.email_change_allowed(user, &old.email, &email)?;
        let moved = self.db.write(|c| {
            if c.execute("DELETE FROM email_changes WHERE id = ? AND user_id = ?", [challenge, user])? == 0 {
                return Ok(Err(Error::bad_code()));
            }
            if c.query_row("SELECT 1 FROM users WHERE email = ? AND id <> ?", [email.as_str(), user], |_| Ok(())).optional()?.is_some() {
                return Ok(Err(Error::Conflict("that address has a roll account now: nothing changed".into())));
            }
            c.execute("UPDATE users SET email = ? WHERE id = ?", [email.as_str(), user])?;
            Ok::<_, code_sql::Error>(Ok(()))
        })?;
        moved?;
        self.forget(&key)?;
        let mail = Mail {
            to: old.email.clone(),
            subject: "Your roll account moved to another address".into(),
            text: format!(
                "The roll account {} now uses {} instead of this address. Sign-in codes and roll's mail go there from now on.\n\nIf this wasn't you, tell your admin at once: they can disable the account and end its sessions at {}/dashboard/account.\n",
                old.handle, email, self.config.web_url
            ),
        };
        Ok((self.user(user)?, mail))
    }
}
