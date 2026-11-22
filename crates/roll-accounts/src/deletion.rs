//! A person deletes their own account (GitHub's "Delete your account", docs/accounts.md "Deleting
//! your account"): from a fresh session (sudo mode), typing their handle. In one write, every way
//! in and everything personal goes (sessions, links, second factors, passkeys, pending codes,
//! sign-in places, reset requests), and the row stays as a tombstone: no address (it can be
//! invited again), no name, disabled for good, `deleted_at` set. Ids in the audit log and the
//! handle (their roll code owner keeps it) still name it; nothing brings it back. Their tokens
//! die with it (a token acts as its person now: there is none), and their roll code owner is
//! suspended as a disabled person's is. A company's people are its to remove: SCIM-provisioned
//! people and SSO-enforced domains ask their admin. The last admin can't leave roll without one.

use crate::{Accounts, Error, Mail, Result};
use code_sql::{OptionalExtension, params};

/// What a deleted account's rows hold about the person: everything keyed by them.
const THEIRS: [&str; 11] = ["sessions", "identities", "email_codes", "email_changes", "oauth_states", "totp", "recovery_codes", "passkeys", "passkey_challenges", "reset_requests", "sign_in_places"];

/// Acts on a deleted account (`usr_…`) after its deletion's write. An error leaves it to be tried
/// again ([`Accounts::run_deletions`]).
pub type OnDeleted = Box<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;

impl Accounts {
    /// Run `f` for every deleted account, after the deletion's write, until it succeeds.
    pub fn on_deleted(&self, f: OnDeleted) {
        self.on_deleted.write().unwrap_or_else(|e| e.into_inner()).push(f);
    }

    /// Act on deletions not yet done: each claimed for a minute in a write, so two nodes don't run
    /// one at once, done once every hook took it, and given back when one failed. rolld also calls this now and then, for what
    /// a crash left. Blocking.
    pub fn run_deletions(&self) {
        if self.on_deleted.read().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        let Ok(now) = self.db.now_ms() else { return };
        let claimed = self.db.write(|c| {
            let due: Vec<String> = c.query("SELECT user_id FROM account_deletions WHERE done = 0 AND (claimed_at IS NULL OR claimed_at < ?) ORDER BY at LIMIT 50", [now - 60_000], |r| r.get(0))?;
            for u in &due {
                c.execute("UPDATE account_deletions SET claimed_at = ? WHERE user_id = ?", params![now, u])?;
            }
            Ok::<_, code_sql::Error>(due)
        });
        let Ok(claimed) = claimed else { return };
        let hooks = self.on_deleted.read().unwrap_or_else(|e| e.into_inner());
        for user in claimed {
            let mut ok = true;
            for f in hooks.iter() {
                if let Err(e) = f(&user) {
                    tracing::warn!(%user, error = %e, "accounts: a deleted account's hook failed; it's tried again");
                    ok = false;
                }
            }
            // Done, or given back for the next run to try again.
            let sql = if ok { "UPDATE account_deletions SET done = 1 WHERE user_id = ?" } else { "UPDATE account_deletions SET claimed_at = NULL WHERE user_id = ?" };
            let _ = self.db.write(|c| c.execute(sql, [user.as_str()]));
        }
    }

    /// Delete `user`'s account, `confirm` being their handle. Returns the mail for the address
    /// it had.
    pub fn delete_self(&self, user: &str, confirm: &str) -> Result<Mail> {
        let me = self.user(user)?;
        if confirm.trim() != me.handle {
            return Err(Error::BadRequest(format!("type your handle ({}) to confirm", me.handle), Some("confirm")));
        }
        if self.db.read(|c| c.query_row("SELECT 1 FROM scim_users WHERE user_id = ? LIMIT 1", [user], |_| Ok(())).optional())?.is_some() {
            return Err(Error::Forbidden("your company's provider (SCIM) manages your account: ask your admin to remove it".into(), "managed"));
        }
        if let Some(name) = self.sso_required_for(&me.email)? {
            return Err(Error::Forbidden(format!("{name} manages your account: ask your admin to remove it"), "sso_required"));
        }
        let now = self.db.now_ms()?;
        let tombstone = format!("{user}@deleted.invalid");
        let done = self.db.write(|c| {
            let row = c.query_row("SELECT admin FROM users WHERE id = ? AND deleted_at IS NULL", [user], |r| r.get::<_, bool>(0)).optional()?;
            let Some(admin) = row else { return Ok(Err(Error::NotFound(format!("no user {user}")))) };
            if admin {
                let others: i64 = c.query_row("SELECT COUNT(*) FROM users WHERE admin = 1 AND disabled = 0 AND deleted_at IS NULL AND id <> ?", [user], |r| r.get(0))?;
                if others == 0 {
                    return Ok(Err(Error::Conflict("you're roll's last admin: make someone else an admin first".into())));
                }
            }
            for table in THEIRS {
                c.execute(&format!("DELETE FROM {table} WHERE user_id = ?"), [user])?;
            }
            c.execute(
                "UPDATE users SET email = ?, name = '', admin = 0, disabled = 1, disabled_by = 'self', deleted_at = ? WHERE id = ?",
                params![tombstone, now, user],
            )?;
            // The outbox: other products act on it after this write, until each has.
            c.execute("INSERT INTO account_deletions (user_id, at) VALUES (?, ?) ON CONFLICT (user_id) DO NOTHING", params![user, now])?;
            Ok::<_, code_sql::Error>(Ok(()))
        })?;
        done?;
        self.run_deletions();
        // Their roll code owner's own tokens and machine links (another database: after the write).
        if let Some(owners) = &self.owners
            && let Some(owner) = self.db.read(|c| c.query_row("SELECT code_owner_id FROM users WHERE id = ?", [user], |r| r.get::<_, Option<String>>(0)))?
        {
            owners.suspend(&owner).map_err(|e| Error::Internal(format!("the account is deleted, but cutting off its roll code owner failed: {e}")))?;
        }
        Ok(Mail {
            to: me.email,
            subject: "Your roll account was deleted".into(),
            text: format!(
                "The roll account {} was deleted, as you asked from your account page. Its sessions, sign-in links, second factors and tokens stopped working at once, and this address no longer signs in.\n\nIf this wasn't you, tell your admin at once: someone had your session and confirmed it was you.\n",
                me.handle
            ),
        })
    }
}
