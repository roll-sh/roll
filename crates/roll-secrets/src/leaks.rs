//! Leaked secrets (docs/secrets.md, "Leaked secrets don't land"): which of a project's stored
//! values appear among some text's candidate tokens, found by fingerprint, so nothing is
//! decrypted and no value leaves this module.

use crate::access::{Action, Caller, Scope};
use crate::events::{Event, Kind};
use crate::{Error, Result, Secrets};
use code_sql::OptionalExtension;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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

/// A candidate that is a stored value, and where the value is: its project (another's, for a
/// folder shared with this one) and that project's environment.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Match {
    index: usize,
    leak: Leak,
    project: String,
    env: String,
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
        self.scan_as(project, candidates, caller, false)
    }

    /// [`Secrets::scan`], from a credential guard (roll MCP's, checking what a tool call would write:
    /// `X-Roll-Guard: 1`): the same match, audited as `secrets.guard` so those rows can be told apart.
    pub fn scan_as(&self, project: &str, candidates: &[String], caller: &Caller, guard: bool) -> Result<Vec<Found>> {
        caller.need(Action::Read, &Scope::project(project))?;
        if candidates.len() > SCAN_MAX {
            return Err(Error::BadRequest(format!("at most {SCAN_MAX} candidates a scan")));
        }
        self.project(project)?;
        // Shared values and the caller's own personal ones: never someone else's personal value,
        // which no other caller ever sees.
        let found: Vec<Found> = self
            .matches(project, candidates, Some(&caller.principal), caller.runtime)?
            .into_iter()
            .filter(|m| !m.leak.environment.contains('/') && caller.allows(Action::Reveal, &Scope::at(project, &m.leak.environment, &m.leak.path)))
            .map(|m| Found { index: m.index, leak: m.leak })
            .collect();
        let action = if guard { "secrets.guard" } else { "secrets.scan" };
        let summary = format!("scanned {} candidates: {} matched", candidates.len(), found.len());
        if found.is_empty() {
            self.audit_now(project, caller, action, None, None, &[], &summary);
            return Ok(found);
        }
        // A row per folder it matched in, each the feed's `scan.finding`: a reader of the feed
        // sees only the environments it may read.
        let mut at: BTreeMap<(&str, &str), BTreeSet<String>> = BTreeMap::new();
        for f in &found {
            at.entry((f.leak.environment.as_str(), f.leak.path.as_str())).or_default().insert(f.leak.key.clone());
        }
        for ((env, path), keys) in at {
            let keys: Vec<String> = keys.into_iter().collect();
            let event = Event::new(Kind::ScanFinding, json!({ "candidates": candidates.len(), "matched": found.len(), "guard": guard }));
            self.audit_now_event(project, caller, action, Some(env), Some(path), &keys, &summary, Some(event));
        }
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
        // roll code's scans: a sensitive value pushed into code leaked like any other.
        Ok(self.matches(project, candidates, None, true)?.into_iter().map(|m| (m.index, m.leak)).collect())
    }

    /// [`leak_matches`](Self::leak_matches) for roll code's checks of `repo` (a change, or a scan
    /// of the repo): each folder a value leaked from gets a `secrets.leaked` row (the feed's
    /// `secret.leaked`) in the project that holds it, once a day for the same keys and repo (the
    /// same change is checked again whenever it's read anew).
    pub fn leaks_in_code(&self, project: &str, candidates: &[String], repo: &str) -> Result<Vec<(usize, Leak)>> {
        let found = self.matches(project, candidates, None, true)?;
        let mut at: BTreeMap<(&str, &str, &str), BTreeSet<String>> = BTreeMap::new();
        for m in &found {
            at.entry((m.project.as_str(), m.env.as_str(), m.leak.path.as_str())).or_default().insert(m.leak.key.clone());
        }
        if !at.is_empty() {
            let (mac, now) = (self.keys()?, self.db.now_ms()?);
            let code = Caller::consumer("roll:code");
            for ((holder, env, path), keys) in at {
                let keys: Vec<String> = keys.into_iter().collect();
                let mark = format!("leak:{repo}:{env}:{path}:{}", keys.join(","));
                let mut data = json!({ "repo": repo });
                if holder != project {
                    data["project"] = json!(project);
                }
                let summary = format!("{} found in roll code repo {repo}: rotate and remove it from the code", keys.join(", "));
                let res = self.db.write(|c| {
                    if crate::events::fresh(c, holder, &mark, now)? {
                        crate::audit::record_event(c, mac, holder, &code, "secrets.leaked", Some(env), Some(path), &keys, &summary, now, Event::new(Kind::SecretLeaked, data.clone()))?;
                    }
                    Ok::<_, Error>(())
                });
                if let Err(e) = res {
                    tracing::warn!(project = holder, repo, "recording a leak found in code failed: {e}");
                }
            }
        }
        Ok(found.into_iter().map(|m| (m.index, m.leak)).collect())
    }

    /// The matches, among personal values only `owner`'s when one is given; sensitive values only
    /// with `sensitive` (never a test of a person's guess against one).
    fn matches(&self, project: &str, candidates: &[String], owner: Option<&str>, sensitive: bool) -> Result<Vec<Match>> {
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
                "SELECT s.key, e.slug, s.path, s.fp, s.project_id, s.tags FROM secrets s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL
                 WHERE s.value IS NOT NULL AND (CAST(? AS TEXT) IS NULL OR s.owner = '' OR s.owner = ?)
                   AND (s.project_id = ? OR (s.owner = '' AND EXISTS (SELECT 1 FROM exports x WHERE x.env_id = s.env_id AND x.path = s.path AND x.to_project = ?)))",
                code_sql::params![owner, owner, project, project],
                |r| Ok((Leak { key: r.get(0)?, environment: r.get(1)?, path: r.get(2)? }, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)),
            )
        })?;
        let rows: Vec<(Leak, String, String)> = rows.into_iter().filter(|(_, _, _, tags)| sensitive || !crate::names::sensitive(&crate::from_json::<Vec<String>>(tags))).map(|(l, fp, p, _)| (l, fp, p)).collect();
        let mut fps: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut out: Vec<Match> = vec![];
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
            let env = l.environment.clone();
            if p != project {
                let name = self.workspace.get(roll_workspace::Scope::All, &p).map(|x| x.name).unwrap_or_else(|_| p.clone());
                l.environment = format!("{name}/{}", l.environment);
            }
            out.extend(hits.iter().map(|i| Match { index: *i, leak: l.clone(), project: p.clone(), env: env.clone() }));
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}
