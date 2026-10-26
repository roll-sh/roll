//! Throttles of failed attempts (wrong MFA codes, wrong sign-in passwords), kept in the metadata
//! so they survive a restart and hold across a takeover: a node that takes over the log has the
//! counts the old one had.
//!
//! Counts are written with relaxed writes (committed with the next group, without waiting), so
//! counting a failure adds no latency, and a success only reads unless there were failures to
//! forget. A crash may lose the last few counts, which costs a guesser more than it gains.
//!
//! Rows are `iam (account, 'throttle', <what was attempted>)`; callers only count attempts at
//! things that exist (a device, a user), so nobody can grow the table with made-up names.

use crate::error::Result;
use crate::iam::kind;
use crate::store::Store;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Failures {
    /// Failures since `since`.
    n: u32,
    /// When the window started (Unix ms).
    since: i64,
}

impl Store {
    fn failures(&self, account: &str, key: &str) -> Result<Option<Failures>> {
        let v: Option<String> = self.read(|c| Ok(c.prepare_cached("SELECT value FROM iam WHERE account_id = ?1 AND kind = ?2 AND name = ?3")?.query_row(params![account, kind::THROTTLE, key], |r| r.get(0)).optional()?))?;
        Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// Whether `key` (in `account`) has failed `max` times within the last `window_ms`.
    pub fn throttled(&self, account: &str, key: &str, max: u32, window_ms: i64) -> Result<bool> {
        let now = self.now();
        Ok(self.failures(account, key)?.is_some_and(|f| f.n >= max && now - f.since < window_ms))
    }

    /// Count a failure of `key` (a window of `window_ms` starts with the first one).
    pub fn record_failure(&self, account: &str, key: &str, window_ms: i64) -> Result<()> {
        let now = self.now();
        self.write_relaxed(|c| {
            let have: Option<String> = c.prepare_cached("SELECT value FROM iam WHERE account_id = ?1 AND kind = ?2 AND name = ?3")?.query_row(params![account, kind::THROTTLE, key], |r| r.get(0)).optional()?;
            let mut f: Failures = have.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or(Failures { n: 0, since: now });
            if now - f.since >= window_ms {
                f = Failures { n: 0, since: now };
            }
            f.n = f.n.saturating_add(1);
            c.execute(
                "INSERT INTO iam (account_id, kind, name, value) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (account_id, kind, name) DO UPDATE SET value = excluded.value",
                params![account, kind::THROTTLE, key, serde_json::to_string(&f)?],
            )?;
            Ok(())
        })
    }

    /// Forget `key`'s failures (after a success). Reads only, unless there were some.
    pub fn clear_failures(&self, account: &str, key: &str) -> Result<()> {
        if self.failures(account, key)?.is_none() {
            return Ok(());
        }
        self.write_relaxed(|c| {
            c.execute("DELETE FROM iam WHERE account_id = ?1 AND kind = ?2 AND name = ?3", params![account, kind::THROTTLE, key])?;
            Ok(())
        })
    }
}
