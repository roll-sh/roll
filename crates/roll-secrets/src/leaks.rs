//! Leaked secrets (docs/secrets.md, "Leaked secrets don't land"): which of a project's stored
//! values appear among some text's candidate tokens, found by fingerprint, so nothing is
//! decrypted and no value leaves this module.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets};
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

/// The most candidates one scan takes (`POST …/leaks`).
pub const SCAN_MAX: usize = 20_000;

/// A candidate of a scan that is a stored value: its index, and where the value is.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    pub index: usize,
    #[serde(flatten)]
    pub leak: Leak,
}

impl Secrets {
    /// A caller's scan of text of its own (`roll secrets scan`, a pre-commit hook): which of
    /// `candidates` are values of this project's it may reveal. Only those: anything else would
    /// make the scan an oracle for guessing values it can't read.
    pub fn scan(&self, project: &str, candidates: &[String], caller: &Caller) -> Result<Vec<Found>> {
        caller.need(Action::Read, &Scope::project(project))?;
        if candidates.len() > SCAN_MAX {
            return Err(Error::BadRequest(format!("at most {SCAN_MAX} candidates a scan")));
        }
        self.project(project)?;
        // Shared values and the caller's own personal ones: never someone else's personal value,
        // which no other caller ever sees.
        let found: Vec<Found> = self
            .matches(project, candidates, Some(&caller.principal))?
            .into_iter()
            .filter(|(_, l)| !l.environment.contains('/') && caller.allows(Action::Reveal, &Scope::at(project, &l.environment, &l.path)))
            .map(|(index, leak)| Found { index, leak })
            .collect();
        let keys: Vec<String> = found.iter().map(|f| f.leak.key.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        self.audit_now(project, caller, "secrets.scan", None, None, &keys, &format!("scanned {} candidates: {} matched", candidates.len(), found.len()));
        Ok(found)
    }

    /// The secrets of `project`, in any environment, whose value equals one of `candidates`.
    /// Only rows that hold a value count (plain values and managed links): live and dynamic links
    /// store none. A project that never had secrets has none to leak (and isn't set up by asking).
    pub fn leaks(&self, project: &str, candidates: &[String]) -> Result<Vec<Leak>> {
        let found: BTreeSet<Leak> = self.leak_matches(project, candidates)?.into_iter().map(|(_, l)| l).collect();
        Ok(found.into_iter().collect())
    }

    /// [`leaks`](Self::leaks) with which candidate (its index) each one was: a scan says where.
    /// Personal values count too: one pushed into code leaked as much as a shared one.
    pub fn leak_matches(&self, project: &str, candidates: &[String]) -> Result<Vec<(usize, Leak)>> {
        self.matches(project, candidates, None)
    }

    /// The matches, among personal values only `owner`'s when one is given.
    fn matches(&self, project: &str, candidates: &[String], owner: Option<&str>) -> Result<Vec<(usize, Leak)>> {
        if !candidates.iter().any(|c| c.chars().count() >= MIN_LEN) {
            return Ok(Vec::new());
        }
        let known = self.db.read(|c| c.query_row("SELECT 1 FROM projects WHERE project_id = ?", [project], |r| r.get::<_, i64>(0)).optional())?;
        if known.is_none() {
            return Ok(Vec::new());
        }
        // Its own values (personal ones too), and the shared values other projects share with it,
        // each fingerprinted with its own project's key: candidates are fingerprinted once per
        // project that has rows to match.
        let rows = self.db.read(|c| {
            c.query(
                "SELECT s.key, e.slug, s.path, s.fp, s.project_id FROM secrets s JOIN environments e ON e.id = s.env_id
                 WHERE s.value IS NOT NULL AND (CAST(? AS TEXT) IS NULL OR s.owner = '' OR s.owner = ?)
                   AND (s.project_id = ? OR (s.owner = '' AND EXISTS (SELECT 1 FROM exports x WHERE x.env_id = s.env_id AND x.path = s.path AND x.to_project = ?)))",
                code_sql::params![owner, owner, project, project],
                |r| Ok((Leak { key: r.get(0)?, environment: r.get(1)?, path: r.get(2)? }, r.get::<_, String>(3)?, r.get::<_, String>(4)?)),
            )
        })?;
        let mut fps: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut out: Vec<(usize, Leak)> = vec![];
        for (mut l, fp, p) in rows {
            if !fps.contains_key(&p) {
                let dek = self.project(&p)?;
                let mut by_fp: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, c) in candidates.iter().enumerate().filter(|(_, c)| c.chars().count() >= MIN_LEN) {
                    by_fp.entry(dek.fingerprint(c)).or_default().push(i);
                }
                fps.insert(p.clone(), by_fp);
            }
            let Some(hits) = fps[&p].get(&fp) else { continue };
            if p != project {
                let name = self.workspace.get(&p).map(|x| x.name).unwrap_or_else(|_| p.clone());
                l.environment = format!("{name}/{}", l.environment);
            }
            out.extend(hits.iter().map(|i| (*i, l.clone())));
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}
