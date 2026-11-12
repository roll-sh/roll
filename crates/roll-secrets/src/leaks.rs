//! Leaked secrets (docs/secrets.md, "Leaked secrets don't land"): which of a project's stored
//! values appear among some text's candidate tokens, found by fingerprint, so nothing is
//! decrypted and no value leaves this module.

use crate::{Result, Secrets};
use code_sql::OptionalExtension;
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};

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
        let candidates: Vec<&String> = candidates.iter().filter(|c| c.chars().count() >= MIN_LEN).collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let known = self.db.read(|c| c.query_row("SELECT 1 FROM projects WHERE project_id = ?", [project], |r| r.get::<_, i64>(0)).optional())?;
        if known.is_none() {
            return Ok(Vec::new());
        }
        let dek = self.project(project)?;
        let fps: HashSet<String> = candidates.into_iter().map(|c| dek.fingerprint(c)).collect();
        let rows = self.db.read(|c| {
            c.query(
                "SELECT s.key, e.slug, s.path, s.fp FROM secrets s JOIN environments e ON e.id = s.env_id WHERE s.project_id = ? AND s.value IS NOT NULL",
                [project],
                |r| Ok((Leak { key: r.get(0)?, environment: r.get(1)?, path: r.get(2)? }, r.get::<_, String>(3)?)),
            )
        })?;
        let found: BTreeSet<Leak> = rows.into_iter().filter(|(_, fp)| fps.contains(fp)).map(|(l, _)| l).collect();
        Ok(found.into_iter().collect())
    }
}
