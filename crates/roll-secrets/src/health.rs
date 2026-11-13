//! A project's health (docs/secrets.md, "Health"): everything insights flag, in every standard
//! environment and folder at once, worst first, so nothing waits on someone opening the right
//! folder. Each finding names a key where it's set, never a value; nothing is decrypted.

use crate::access::{Action, Caller, Scope};
use crate::envs::{self, Kind};
use crate::insights::EXPIRING_DAYS;
use crate::{Result, Secrets, from_json};
use code_sql::params;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Reads of values over this many days count as use.
const UNREAD_DAYS: i64 = 90;
const READS_MAX: i64 = 100_000;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    /// Its value stopped working outside roll.
    Expired,
    /// A required key an environment doesn't define.
    Missing,
    /// A protected environment's value, also set in an unprotected one.
    Exposed,
    /// A credential easy to guess (shown to whoever may reveal it).
    Weak,
    /// Its value stops working outside roll within two weeks.
    Expiring,
    /// Unchanged for longer than the environment's `max_age_days`.
    Stale,
    /// Nobody read it in 90 days.
    Unread,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Finding {
    pub issue: Issue,
    pub environment: String,
    pub path: String,
    pub key: String,
    /// What exactly (the other environments, the reason, the date).
    pub detail: String,
}

impl Secrets {
    /// Every finding in the project's standard environments the caller may read, worst first.
    pub fn project_health(&self, project: &str, caller: &Caller) -> Result<Vec<Finding>> {
        caller.need(Action::Read, &Scope::project(project))?;
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let empty = dek.fingerprint("");
        type Row = (String, String, String, String, Option<String>, Option<String>, i64, Option<i64>, Option<String>);
        let (envs, rows, reads) = self.db.read(|c| {
            let envs: Vec<envs::EnvRow> = envs::all(c, project)?.into_iter().filter(|e| e.kind == Kind::Standard).collect();
            let rows: Vec<Row> = c.query(
                "SELECT env_id, path, key, fp, link, refs, updated_at, expires_at, weak FROM secrets WHERE project_id = ? AND owner = '' ORDER BY path, key",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )?;
            let reads: Vec<(String, String, String)> = c.query(
                "SELECT env, path, keys FROM audit WHERE project_id = ? AND at >= ? AND action IN ('secrets.reveal', 'secrets.export', 'secrets.resolve', 'sync.push') ORDER BY id DESC LIMIT ?",
                params![project, now - UNREAD_DAYS * 86_400_000, READS_MAX],
                |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?)),
            )?;
            Ok::<_, crate::Error>((envs, rows, reads))
        })?;
        let slug: HashMap<&str, &envs::EnvRow> = envs.iter().map(|e| (e.id.as_str(), e)).collect();
        let mut read: HashSet<(String, String, String)> = HashSet::new();
        for (env, path, keys) in reads {
            for k in from_json::<Vec<String>>(&keys) {
                read.insert((env.clone(), path.clone(), k));
            }
        }
        // Plain values of unprotected environments, by fingerprint: where a protected one's also is.
        let mut unprotected: HashMap<&str, BTreeSet<&str>> = HashMap::new();
        for (env_id, _, _, fp, link, refs, ..) in &rows {
            if let Some(e) = slug.get(env_id.as_str())
                && !e.protected
                && link.is_none()
                && refs.as_deref().is_none_or(|r| r == "[]")
                && *fp != empty
            {
                unprotected.entry(fp.as_str()).or_default().insert(e.slug.as_str());
            }
        }
        let mut out = vec![];
        let mut have: HashSet<(String, String, String)> = HashSet::new();
        for (env_id, path, key, fp, link, refs, updated_at, expires_at, weak) in &rows {
            let Some(e) = slug.get(env_id.as_str()) else { continue };
            have.insert((e.slug.clone(), path.clone(), key.clone()));
            if !caller.allows(Action::Read, &Scope::at(project, &e.slug, path)) {
                continue;
            }
            let mut add = |issue: Issue, detail: String| out.push(Finding { issue, environment: e.slug.clone(), path: path.clone(), key: key.clone(), detail });
            let day = |ms: i64| chrono_day(ms);
            if let Some(t) = expires_at {
                if *t <= now {
                    add(Issue::Expired, format!("expired {}", day(*t)));
                } else if *t < now + EXPIRING_DAYS * 86_400_000 {
                    add(Issue::Expiring, format!("expires {}", day(*t)));
                }
            }
            if let Some(w) = weak
                && caller.allows(Action::Reveal, &Scope::at(project, &e.slug, path))
            {
                add(Issue::Weak, w.clone());
            }
            if e.protected
                && link.is_none()
                && refs.as_deref().is_none_or(|r| r == "[]")
                && *fp != empty
                && let Some(others) = unprotected.get(fp.as_str())
            {
                let named: Vec<&str> = others.iter().copied().filter(|o| caller.allows(Action::Read, &Scope::at(project, o, path))).collect();
                if !named.is_empty() {
                    add(Issue::Exposed, format!("also in {}", named.join(", ")));
                }
            }
            let changes_alone = link.as_deref().is_none_or(|l| l.contains("\"mode\":\"managed\""));
            if let Some(d) = e.max_age_days
                && changes_alone
                && now - updated_at > d * 86_400_000
            {
                add(Issue::Stale, format!("unchanged for over {d} days"));
            }
            if !read.contains(&(e.slug.clone(), path.clone(), key.clone())) && now - updated_at > UNREAD_DAYS * 86_400_000 {
                add(Issue::Unread, format!("not read in {UNREAD_DAYS} days"));
            }
        }
        // Required keys a standard environment doesn't define (its imports and base included).
        for r in self.required(project, caller)? {
            for e in &envs {
                if have.contains(&(e.slug.clone(), r.path.clone(), r.key.clone())) || !caller.allows(Action::Read, &Scope::at(project, &e.slug, &r.path)) {
                    continue;
                }
                let seen = self.view(project, &e.slug, &r.path, caller, false, Action::Read)?.iter().any(|x| x.row.key == r.key);
                if !seen {
                    out.push(Finding { issue: Issue::Missing, environment: e.slug.clone(), path: r.path.clone(), key: r.key.clone(), detail: "required".into() });
                }
            }
        }
        let position: HashMap<&str, i64> = envs.iter().map(|e| (e.slug.as_str(), e.position)).collect();
        out.sort_by(|a, b| (a.issue, position.get(a.environment.as_str()), &a.path, &a.key).cmp(&(b.issue, position.get(b.environment.as_str()), &b.path, &b.key)));
        Ok(out)
    }
}

/// 2027-03-31 (UTC).
fn chrono_day(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn days_are_civil_dates() {
        assert_eq!(super::chrono_day(0), "1970-01-01");
        assert_eq!(super::chrono_day(1_869_609_600_000), "2029-03-31");
        assert_eq!(super::chrono_day(951_782_400_000), "2000-02-29");
    }
}
