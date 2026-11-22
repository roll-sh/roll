//! A person lost rights in roll (docs/accounts.md, "Offboarding"): an admin or SCIM disabled them,
//! an admin made them no longer an admin, or they deleted their account. Sessions end in that
//! write, and tokens stop at their next use; what was handed out on their say and checks nobody
//! later (other products' leases, shares, sandboxes holding copied values) is other products' to
//! end. The write records it in an outbox (`account_offboardings`, one row per person), and the
//! hooks ([`Accounts::on_offboarded`]) run from it after the write, and again until each succeeds.
//! A re-enabled account gets nothing back: what was ended stays ended.

use crate::Accounts;
use code_sql::{Conn, params};

/// Acts on someone who lost rights (`usr_…`) after the write. An error leaves it to be tried again.
pub type OnOffboarded = Box<dyn Fn(&str) -> std::result::Result<(), String> + Send + Sync>;

/// Record in the write that changed them that `user` lost rights (again, if it was done).
pub(crate) fn record(c: &Conn<'_>, user: &str, now: i64) -> code_sql::Result<()> {
    c.execute(
        "INSERT INTO account_offboardings (user_id, at, claimed_at, done) VALUES (?, ?, NULL, 0)
         ON CONFLICT (user_id) DO UPDATE SET at = excluded.at, claimed_at = NULL, done = 0",
        params![user, now],
    )?;
    Ok(())
}

impl Accounts {
    /// Run `f` for everyone who loses rights, after the write, until it succeeds.
    pub fn on_offboarded(&self, f: OnOffboarded) {
        self.on_offboarded.write().unwrap_or_else(|e| e.into_inner()).push(f);
    }

    /// Act on offboardings not yet done: each claimed for a minute in a write, so two nodes don't
    /// run one at once, done once every hook took it (unless it was recorded again meanwhile), and
    /// given back when one failed. rolld also calls this now and then, for what a crash left.
    /// Blocking.
    pub fn run_offboardings(&self) {
        if self.on_offboarded.read().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        let Ok(now) = self.db.now_ms() else { return };
        let claimed = self.db.write(|c| {
            let due: Vec<String> = c.query("SELECT user_id FROM account_offboardings WHERE done = 0 AND (claimed_at IS NULL OR claimed_at < ?) ORDER BY at LIMIT 50", [now - 60_000], |r| r.get(0))?;
            for u in &due {
                c.execute("UPDATE account_offboardings SET claimed_at = ? WHERE user_id = ?", params![now, u])?;
            }
            Ok::<_, code_sql::Error>(due)
        });
        let Ok(claimed) = claimed else { return };
        let hooks = self.on_offboarded.read().unwrap_or_else(|e| e.into_inner());
        for user in claimed {
            let mut ok = true;
            for f in hooks.iter() {
                if let Err(e) = f(&user) {
                    tracing::warn!(%user, error = %e, "accounts: an offboarded person's hook failed; it's tried again");
                    ok = false;
                }
            }
            // Only our claim: a row recorded again meanwhile has no claim, and runs again.
            let sql = if ok { "UPDATE account_offboardings SET done = 1 WHERE user_id = ? AND claimed_at = ?" } else { "UPDATE account_offboardings SET claimed_at = NULL WHERE user_id = ? AND claimed_at = ?" };
            let _ = self.db.write(|c| c.execute(sql, params![user, now]));
        }
    }
}
