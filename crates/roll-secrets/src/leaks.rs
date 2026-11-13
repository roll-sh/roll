//! Leaked secrets (docs/secrets.md, "Leaked secrets don't land"): which of a project's stored
//! values appear among some text's candidate tokens, found by fingerprint, so nothing is
//! decrypted and no value leaves this module.

use crate::{Result, Secrets};
use code_sql::OptionalExtension;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};

/// Candidates shorter than this (in characters) are never matched: `true` and `8080` are values
/// too.
pub const MIN_LEN: usize = 8;

/// A stored secret whose value is one of the candidates. Never the value itself.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Leak {
    pub key: String,
    /// The environment's slug.
    pub environment: String,
    pub path: String,
}

impl Secrets {
    /// The secrets of `project`, in any environment, whose value equals one of `candidates`.
    /// Only rows that hold a value count (plain values and managed links): live and dynamic links
    /// store none. A project that never had secrets has none to leak (and isn't set up by asking).
    pub fn leaks(&self, project: &str, candidates: &[String]) -> Result<Vec<Leak>> {
        let found: BTreeSet<Leak> = self.leak_matches(project, candidates)?.into_iter().map(|(_, l)| l).collect();
        Ok(found.into_iter().collect())
    }

    /// [`leaks`](Self::leaks) with which candidate (its index) each one was: a scan says where.
    pub fn leak_matches(&self, project: &str, candidates: &[String]) -> Result<Vec<(usize, Leak)>> {
        if !candidates.iter().any(|c| c.chars().count() >= MIN_LEN) {
            return Ok(Vec::new());
        }
        let known = self.db.read(|c| c.query_row("SELECT 1 FROM projects WHERE project_id = ?", [project], |r| r.get::<_, i64>(0)).optional())?;
        if known.is_none() {
            return Ok(Vec::new());
        }
        let dek = self.project(project)?;
        let mut by_fp: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in candidates.iter().enumerate().filter(|(_, c)| c.chars().count() >= MIN_LEN) {
            by_fp.entry(dek.fingerprint(c)).or_default().push(i);
        }
        let rows = self.db.read(|c| {
            c.query(
                "SELECT s.key, e.slug, s.path, s.fp FROM secrets s JOIN environments e ON e.id = s.env_id WHERE s.project_id = ? AND s.value IS NOT NULL",
                [project],
                |r| Ok((Leak { key: r.get(0)?, environment: r.get(1)?, path: r.get(2)? }, r.get::<_, String>(3)?)),
            )
        })?;
        let mut out: Vec<(usize, Leak)> = rows.into_iter().flat_map(|(l, fp)| by_fp.get(&fp).into_iter().flatten().map(move |i| (*i, l.clone()))).collect();
        out.sort();
        out.dedup();
        Ok(out)
    }
}
