//! Logins that take turns for one managed credential: a database's two roles (outside.rs), a
//! directory's accounts (directories.rs). Each issue sets a new password on the login not handed
//! out last and hands it out, while the other keeps working through the grace period; then its
//! password is replaced by one nobody knows. A login's generation travels in each issued
//! credential's handle, so a late retirement never undoes the login's next issue.
//!
//! With one login there is no other to take the turn: each issue replaces its password at once,
//! and the retirement after the grace finds a newer generation and does nothing.

use crate::{Error, Result, Value};
use code_sql::{Db, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

/// Which login is handed out next, and each login's generation.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    next: usize,
    generation: BTreeMap<String, u64>,
    /// Logins an issue or a retirement is changing right now, until (unix ms): nothing else takes them.
    #[serde(default)]
    held: BTreeMap<String, i64>,
}

/// How long an issue or a retirement holds a login: past its connection's timeout (15 s) and the
/// write that stores what it issued, after which the stored credential or lease keeps it busy. A
/// hold outlives a crash between the two by no more than this.
const HOLD_MS: i64 = 2 * 60_000;

/// One registry whose rows take turns: its table (with `id` and a `state` column), its link kind,
/// and the handle's field naming the login.
pub(crate) struct Turns {
    pub table: &'static str,
    pub kind: &'static str,
    pub key: &'static str,
}

impl Turns {
    fn state(&self, c: &code_sql::Conn<'_>, id: &str) -> Result<State> {
        let s: String = c.query_row(&format!("SELECT state FROM {} WHERE id = ?", self.table), [id], |r| r.get(0))?;
        Ok(serde_json::from_str(&s).unwrap_or_default())
    }

    fn save(&self, c: &code_sql::Conn<'_>, id: &str, st: &State) -> Result<()> {
        c.execute(&format!("UPDATE {} SET state = ? WHERE id = ?", self.table), params![serde_json::to_string(st).expect("state"), id])?;
        Ok(())
    }

    /// The logins nobody may take now: the one a stored credential uses, any an unrevoked lease
    /// still covers (a retired one in its grace), and any held by an issue or retirement in flight.
    fn busy(&self, c: &code_sql::Conn<'_>, id: &str, st: &State, now: i64) -> Result<BTreeSet<String>> {
        let like = (format!("%\"kind\":\"{}\"%", self.kind), format!("%\"ref\":\"{id}\"%"));
        let mut out: BTreeSet<String> = st.held.iter().filter(|(_, until)| **until > now).map(|(r, _)| r.clone()).collect();
        let stored: Vec<String> = c.query("SELECT link_state FROM secrets WHERE link_state IS NOT NULL AND link LIKE ? AND link LIKE ?", params![like.0, like.1], |r| r.get(0))?;
        let leased: Vec<String> = c.query("SELECT handle FROM leases WHERE revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![like.0, like.1], |r| r.get(0))?;
        for (text, inner) in stored.iter().map(|t| (t, true)).chain(leased.iter().map(|t| (t, false))) {
            let v: Json = serde_json::from_str(text).unwrap_or_default();
            let handle = if inner { &v["handle"] } else { &v };
            if let Some(login) = handle[self.key].as_str() {
                out.insert(login.to_string());
            }
        }
        Ok(out)
    }

    /// `login` (at `generation`, unless another issue took it since) is next to hand out.
    fn free(&self, db: &Db, id: &str, login: &str, generation: u64, idx: usize, to: u64) -> Result<()> {
        db.write(|c| {
            let mut st = self.state(c, id)?;
            if st.generation.get(login).copied() == Some(generation) {
                st.next = idx;
                st.generation.insert(login.to_string(), to);
                st.held.remove(login);
                self.save(c, id, &st)?;
            }
            Ok(())
        })
    }

    /// Hand out the next login with a new password: claim a free one first (the database only),
    /// then `set` its password. Returns the login, its password and the handle.
    pub fn issue(&self, db: &Db, id: &str, logins: &[String], password: impl Fn() -> String, set: impl Fn(&str, &str) -> std::result::Result<(), String>) -> Result<(String, Value, Json)> {
        let alone = logins.len() == 1;
        let now = db.now_ms()?;
        let (idx, login, generation) = db.write(|c| {
            let mut st = self.state(c, id)?;
            // One login is always the one in use: only an issue or retirement in flight stops it
            // (its hold also keeps two rotations from storing each other's password).
            let busy = if alone { st.held.iter().filter(|(_, until)| **until > now).map(|(r, _)| r.clone()).collect() } else { self.busy(c, id, &st, now)? };
            let order = if alone { vec![0] } else { vec![st.next % 2, (st.next + 1) % 2] };
            let idx = order.into_iter().find(|i| !busy.contains(&logins[*i])).ok_or_else(|| {
                let what = if alone { format!("{} is being changed by another rotation: try again in a minute", logins[0]) } else { format!("both of {} are in use: one serves the current login, the other is still in its grace period (or another rotation is running). Rotate again once the grace ends", logins.join(" and ")) };
                Error::Conflict("in_use", what)
            })?;
            let login = logins[idx].clone();
            st.next = (idx + 1) % logins.len();
            let g = st.generation.entry(login.clone()).or_default();
            *g += 1;
            let generation = *g;
            st.held.insert(login.clone(), now + HOLD_MS);
            self.save(c, id, &st)?;
            Ok::<_, Error>((idx, login, generation))
        })?;
        let pw = Value::new(password());
        if let Err(why) = set(&login, pw.expose()) {
            // The login kept its password: give the turn back, or the next issue would take the
            // one in use.
            self.free(db, id, &login, generation, idx, generation - 1)?;
            return Err(Error::Link(format!("setting {login}'s password: {why}")));
        }
        let handle = json!({ self.key: login, "generation": generation });
        Ok((login, pw, handle))
    }

    /// After the grace period: the old login's password replaced by one nobody knows, unless the
    /// login was handed out again since.
    pub fn retire(&self, db: &Db, id: &str, logins: &[String], handle: &Json, password: impl Fn() -> String, set: impl Fn(&str, &str) -> std::result::Result<(), String>) -> Result<()> {
        let (Some(login), Some(generation)) = (handle[self.key].as_str(), handle["generation"].as_u64()) else { return Ok(()) };
        if db.read(|c| self.state(c, id))?.generation.get(login).copied() != Some(generation) {
            return Ok(());
        }
        // Hold it while its password changes, so no issue takes it halfway.
        let now = db.now_ms()?;
        let held = db.write(|c| {
            let mut st = self.state(c, id)?;
            if st.generation.get(login).copied() != Some(generation) {
                return Ok::<_, Error>(false);
            }
            st.held.insert(login.to_string(), now + HOLD_MS);
            self.save(c, id, &st)?;
            Ok(true)
        })?;
        if !held {
            return Ok(());
        }
        if let Err(why) = set(login, &password()) {
            db.write(|c| {
                let mut st = self.state(c, id)?;
                st.held.remove(login);
                self.save(c, id, &st)
            })?;
            return Err(Error::Link(format!("retiring {login}'s password: {why}")));
        }
        // Its latest handout is gone (a grace that ended, or an issue whose write failed): it's
        // the login to hand out next, never the other, which may be in use.
        let idx = logins.iter().position(|r| r == login).unwrap_or(0);
        self.free(db, id, login, generation, idx, generation)
    }

    /// A fresh state for a new registration.
    pub fn initial() -> String {
        serde_json::to_string(&State::default()).expect("state")
    }
}
