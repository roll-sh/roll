//! Leaked secrets (docs/secrets.md, "Leaked secrets don't land"): which of a project's stored
//! values appear among some text's candidate tokens, found by fingerprint, so nothing is
//! decrypted and no value leaves this module.
//!
//! A leak's response ("When a value leaks"): `secret.leaked` in the project holding the value,
//! and, where the project's `leaks.rotate` is `managed`, each leaked managed credential rotated
//! once through [`Secrets::rotate_leaked`] (the rotate path, with no grace for the leaked
//! credential), or a change request for its rotation where the environment needs approvals. A
//! credential is claimed in `leak_rotations` before anything is issued, so one leak rotates it
//! once, whichever node and however often the same text is checked. A key rotates for a leak at
//! most once an hour, whatever found it, and a caller's report (`leaked: true`) rotates only what
//! the caller could rotate itself: reading a value is never the power to cut it.

use crate::access::{Action, Caller, Scope};
use crate::events::{Event, Kind};
use crate::{Error, Result, Secrets, envs};
use code_sql::{Conn, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS leak_rotations (
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    credential TEXT NOT NULL,
    state TEXT NOT NULL,
    request_id TEXT,
    at INTEGER NOT NULL,
    PRIMARY KEY (env_id, credential)
) WITHOUT ROWID;
";

/// How long a leak rotation that failed, or whose node stopped mid-way, waits before the same
/// leak found again claims it anew.
const RETRY_MS: i64 = 3_600_000;
/// A key rotated for a leak isn't rotated for another within this long, whoever reports it.
const CAP_MS: i64 = 3_600_000;
/// Why a leak didn't rotate a key it otherwise would have (the event's `reason`).
const CAPPED: &str = "rotated within the hour";
/// Who rotates a leaked managed key (the project's `leaks.rotate`, an admin's choice).
pub(crate) const LEAKS: &str = "roll:leaks";

/// What a project does when one of its managed keys leaks (`leaks.rotate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LeakRotate {
    /// Rotated at once, with no grace for the leaked credential.
    Managed,
    /// Only the event (`rotatable: true`), and a Rotate button for a person.
    Off,
}

impl LeakRotate {
    pub fn parse(s: &str) -> Option<LeakRotate> {
        match s {
            "managed" => Some(LeakRotate::Managed),
            "off" => Some(LeakRotate::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LeakRotate::Managed => "managed",
            LeakRotate::Off => "off",
        }
    }
}

/// Where a leak was found: what its `secret.leaked` row says, and its once-a-day mark.
enum Whence {
    /// In a roll code repository (a change's added lines or a scan), of the project `project`.
    Code { repo: String, project: String },
    /// Reported by a caller who holds the value (`POST …/leaks` with `leaked: true`): it rotates
    /// only where the caller may rotate (`write`) itself.
    Report { by: Caller },
}

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
    /// A shared managed link's credential (what one rotation replaces, for every key that shares
    /// it): roll can rotate it. Never a personal value, a plain value or another kind of link.
    credential: Option<String>,
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
        self.scan_as(project, candidates, caller, false, false)
    }

    /// [`Secrets::scan`], from a credential guard (roll MCP's, checking what a tool call would write:
    /// `X-Roll-Guard: 1`): the same match, audited as `secrets.guard` so those rows can be told apart.
    /// With `leaked`, the text was published (a paste, a public repo, a scanner's report): what it
    /// matched is a leak, answered as one found in code is ("When a value leaks"), except that it
    /// rotates a key only where the caller itself may write it.
    pub fn scan_as(&self, project: &str, candidates: &[String], caller: &Caller, guard: bool, leaked: bool) -> Result<Vec<Found>> {
        caller.need(Action::Read, &Scope::project(project))?;
        if candidates.len() > SCAN_MAX {
            return Err(Error::BadRequest(format!("at most {SCAN_MAX} candidates a scan")));
        }
        if guard && leaked {
            return Err(Error::BadRequest("a guard's check is of text not written yet: it reports no leak".into()));
        }
        self.project(project)?;
        // Shared values and the caller's own personal ones: never someone else's personal value,
        // which no other caller ever sees.
        let matched: Vec<Match> = self
            .matches(project, candidates, Some(&caller.principal), caller.runtime)?
            .into_iter()
            .filter(|m| !m.leak.environment.contains('/') && caller.allows(Action::Reveal, &Scope::at(project, &m.leak.environment, &m.leak.path)))
            .collect();
        let found: Vec<Found> = matched.iter().map(|m| Found { index: m.index, leak: m.leak.clone() }).collect();
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
        if leaked {
            self.respond(&matched, &Whence::Report { by: caller.clone() });
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
    /// of the repo), answered as a leak ([`Secrets::respond`]): a `secrets.leaked` row (the
    /// feed's `secret.leaked`) per folder in the project that holds the value, once a day for the
    /// same keys and repo (the same change is checked again whenever it's read anew), and with
    /// `leaks.rotate = managed` its managed keys rotated. Each match says whether a person may
    /// rotate it from the leak's notice: a managed key of a project that doesn't rotate on its own.
    pub fn leaks_in_code(&self, project: &str, candidates: &[String], repo: &str) -> Result<Vec<(usize, Leak, bool)>> {
        let found = self.matches(project, candidates, None, true)?;
        let settings = self.respond(&found, &Whence::Code { repo: repo.to_string(), project: project.to_string() });
        Ok(found
            .into_iter()
            .map(|m| {
                let offer = m.credential.is_some() && settings.get(&m.project) == Some(&LeakRotate::Off);
                (m.index, m.leak, offer)
            })
            .collect())
    }

    /// What `project` does when a managed key leaks.
    pub fn leak_rotate(&self, project: &str) -> Result<LeakRotate> {
        let v: Option<String> = self.db.read(|c| c.query_row("SELECT leak_rotate FROM projects WHERE project_id = ?", [project], |r| r.get::<_, Option<String>>(0)).optional())?.flatten();
        Ok(v.as_deref().and_then(LeakRotate::parse).unwrap_or(LeakRotate::Off))
    }

    /// Set `leaks.rotate` (an admin's; audited as `leaks.rotate`).
    pub fn set_leak_rotate(&self, project: &str, rotate: LeakRotate, caller: &Caller) -> Result<LeakRotate> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("UPDATE projects SET leak_rotate = ? WHERE project_id = ?", params![rotate.as_str(), project])?;
            let summary = match rotate {
                LeakRotate::Managed => "leaked managed keys rotate at once, with no grace for the leaked credential",
                LeakRotate::Off => "leaked managed keys are reported, not rotated",
            };
            crate::audit::record(c, self.keys()?, project, caller, "leaks.rotate", None, None, &[], summary, now)
        })?;
        Ok(rotate)
    }

    /// A leak's response, best effort (the check that found it answers either way): per folder
    /// of the project holding the values, its `secret.leaked` row (once a day for the same keys
    /// and place) saying whether roll can rotate them, and, under `leaks.rotate = managed`, each
    /// leaked credential claimed in the same write, then rotated outside it, or, where the
    /// environment or folder needs approvals, a change request for its rotation opened instead.
    /// A caller's report claims only in folders where it may write, and no key whose credential
    /// rotated for a leak within the hour is claimed again (the event says so instead).
    /// The holding projects' settings, as read.
    fn respond(&self, found: &[Match], whence: &Whence) -> HashMap<String, LeakRotate> {
        let mut settings: HashMap<String, LeakRotate> = HashMap::new();
        if found.is_empty() {
            return settings;
        }
        let (mac, now) = match (self.keys(), self.db.now_ms()) {
            (Ok(k), Ok(n)) => (k, n),
            (Err(e), _) => return warn_unrecorded(e),
            (_, Err(e)) => return warn_unrecorded(e.into()),
        };
        // Per folder: its keys, and its credentials (one rotation each, whichever key it was found by).
        let mut at: BTreeMap<(&str, &str, &str), Leaked<'_>> = BTreeMap::new();
        for m in found {
            let e = at.entry((m.project.as_str(), m.env.as_str(), m.leak.path.as_str())).or_default();
            e.0.insert(m.leak.key.clone());
            if let Some(cred) = &m.credential {
                e.1.entry(cred).or_insert(&m.leak.key);
            }
        }
        let (by, place, mark_place) = match whence {
            Whence::Code { repo, .. } => (Caller::consumer("roll:code"), format!("roll code repo {repo}"), repo.clone()),
            Whence::Report { by } => (by.clone(), format!("text {} reported published", by.principal), "report".to_string()),
        };
        let leaks = Caller::consumer(LEAKS);
        // (project, environment, its id, folder, key, credential), each claimed.
        let mut rotate: Vec<(String, String, String, String, String, String)> = vec![];
        for ((holder, env, path), (keys, creds)) in at {
            let setting = match settings.get(holder) {
                Some(s) => *s,
                None => {
                    let s = self.leak_rotate(holder).unwrap_or(LeakRotate::Off);
                    settings.insert(holder.to_string(), s);
                    s
                }
            };
            let keys: Vec<String> = keys.into_iter().collect();
            let mark = format!("leak:{mark_place}:{env}:{path}:{}", keys.join(","));
            let mut data = match whence {
                Whence::Code { repo, project } if project != holder => json!({ "repo": repo, "project": project }),
                Whence::Code { repo, .. } => json!({ "repo": repo }),
                Whence::Report { .. } => json!({ "reported": true }),
            };
            data["rotatable"] = json!(!creds.is_empty());
            // A report is the reporter's say-so: it rotates only what the reporter could rotate
            // through `…/rotate` (`write` there). Otherwise it's answered as `off` would be.
            let may_rotate = match whence {
                Whence::Code { .. } => true,
                Whence::Report { by } => by.allows(Action::Write, &Scope::at(holder, env, path)),
            };
            let res = self.db.write(|c| {
                // Claims first, so the event says what happens next.
                let mut now_rotating: Vec<(String, String, String)> = vec![];
                let mut requested: Vec<String> = vec![];
                let mut capped: Vec<String> = vec![];
                if setting == LeakRotate::Managed && may_rotate && !creds.is_empty() {
                    let e = envs::by_slug(c, holder, env)?;
                    let approvals = e.needs_request() || !crate::folder_policy::covering(c, &e.id, &[path])?.is_empty();
                    for (&cred, &key) in &creds {
                        if rotated_lately(c, &e.id, cred, path, key, now)? {
                            capped.push(key.to_string());
                            continue;
                        }
                        if !claim(c, holder, &e.id, cred, path, key, now)? {
                            continue;
                        }
                        if approvals {
                            let id = self.open_rotate_request(c, holder, &e, path, key, cred, &leaks, now)?;
                            c.execute("UPDATE leak_rotations SET state = 'requested', request_id = ? WHERE env_id = ? AND credential = ?", params![id, e.id, cred])?;
                            requested.push(id);
                        } else {
                            now_rotating.push((e.id.clone(), cred.to_string(), key.to_string()));
                        }
                    }
                }
                let mut data = data.clone();
                let mut summary = format!("{} found in {place}: rotate and remove it", keys.join(", "));
                if !now_rotating.is_empty() {
                    data["rotating"] = json!(now_rotating.iter().map(|(_, _, k)| k).collect::<Vec<_>>());
                    summary = format!("{} found in {place}: roll rotates it now (leaks.rotate is managed); remove it", keys.join(", "));
                }
                if !requested.is_empty() {
                    data["requests"] = json!(requested);
                    summary = format!("{summary}; {env} needs approvals, so its rotation waits in {}", requested.join(", "));
                }
                if !capped.is_empty() {
                    if now_rotating.is_empty() {
                        data["rotating"] = json!(false);
                    }
                    data["reason"] = json!(CAPPED);
                    summary = format!("{summary}; {} {CAPPED}, so roll doesn't rotate it again yet", capped.join(", "));
                }
                // The event once a day; a claim (above) whenever there's a credential to rotate,
                // and a held one each time (whoever keeps reporting it hears why).
                if crate::events::fresh(c, holder, &mark, now)? || !now_rotating.is_empty() || !requested.is_empty() || !capped.is_empty() {
                    crate::audit::record_event(c, mac, holder, &by, "secrets.leaked", Some(env), Some(path), &keys, &summary, now, Event::new(Kind::SecretLeaked, data))?;
                }
                Ok::<_, Error>(now_rotating)
            });
            match res {
                Ok(claimed) => rotate.extend(claimed.into_iter().map(|(env_id, cred, key)| (holder.to_string(), env.to_string(), env_id, path.to_string(), key, cred))),
                Err(e) => tracing::warn!(project = holder, "recording a leak failed: {e}"),
            }
        }
        // Issuing and revoking: outside any write, each claimed above.
        for (holder, env, env_id, path, key, cred) in rotate {
            let outcome = self.rotate_leaked(&holder, &env, &path, &key, &leaks);
            let state = if outcome.is_ok() { "rotated" } else { "failed" };
            if let Err(e) = self.db.write(|c| c.execute("UPDATE leak_rotations SET state = ?, at = ? WHERE env_id = ? AND credential = ?", params![state, now, env_id, cred])) {
                tracing::warn!(project = %holder, env = %env, %key, "keeping a leak rotation's outcome failed: {e}");
            }
            // A failure is already on the key for health, and on the feed (`rotation.failed`).
            if let Err(e) = outcome {
                tracing::warn!(project = %holder, env = %env, %key, "rotating a leaked key failed: {e}");
            }
        }
        settings
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
                "SELECT s.key, e.slug, s.path, s.fp, s.project_id, s.tags, s.owner, s.link, s.link_state FROM secrets s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL
                 WHERE s.value IS NOT NULL AND (CAST(? AS TEXT) IS NULL OR s.owner = '' OR s.owner = ?)
                   AND (s.project_id = ? OR (s.owner = '' AND EXISTS (SELECT 1 FROM exports x WHERE x.env_id = s.env_id AND x.path = s.path AND x.to_project = ?)))",
                code_sql::params![owner, owner, project, project],
                |r| {
                    let (path, key): (String, String) = (r.get(2)?, r.get(0)?);
                    let credential = credential_of(&r.get::<_, String>(6)?, r.get::<_, Option<String>>(7)?.as_deref(), r.get::<_, Option<String>>(8)?.as_deref(), &path, &key);
                    Ok((Leak { key, environment: r.get(1)?, path }, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, credential))
                },
            )
        })?;
        type Row = (Leak, String, String, Option<String>);
        let rows: Vec<Row> = rows.into_iter().filter(|(_, _, _, tags, _)| sensitive || !crate::names::sensitive(&crate::from_json::<Vec<String>>(tags))).map(|(l, fp, p, _, cred)| (l, fp, p, cred)).collect();
        let mut fps: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut out: Vec<Match> = vec![];
        for (mut l, fp, p, credential) in rows {
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
            out.extend(hits.iter().map(|i| Match { index: *i, leak: l.clone(), project: p.clone(), env: env.clone(), credential: credential.clone() }));
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// Claim a leaked credential's rotation (in the leak's write), found by `path`/`key`: new, or
/// one that failed or whose node stopped more than [`RETRY_MS`] ago. A rotation waiting in a
/// change request is never claimed again: whoever reviews it decides.
fn claim(c: &Conn<'_>, project: &str, env_id: &str, credential: &str, path: &str, key: &str, now: i64) -> Result<bool> {
    Ok(c.execute(
        "INSERT INTO leak_rotations (project_id, env_id, credential, state, request_id, at, path, key) VALUES (?, ?, ?, 'claimed', NULL, ?, ?, ?)
         ON CONFLICT (env_id, credential) DO UPDATE SET state = 'claimed', at = excluded.at, path = excluded.path, key = excluded.key
         WHERE leak_rotations.state IN ('claimed', 'failed') AND leak_rotations.at < ?",
        params![project, env_id, credential, now, path, key, now - RETRY_MS],
    )? == 1)
}

/// Whether a key on `credential` (`path`/`key`, or another key sharing it) rotated for a leak of
/// an earlier credential within [`CAP_MS`], or is rotating now: at most one leak rotation per key
/// an hour, so revealing each new value and reporting it again can't churn the key.
fn rotated_lately(c: &Conn<'_>, env_id: &str, credential: &str, path: &str, key: &str, now: i64) -> Result<bool> {
    let mut on: Vec<(String, String)> = match credential.strip_prefix("cred:") {
        Some(id) => c.query("SELECT path, key FROM secrets WHERE env_id = ? AND owner = '' AND link_state LIKE ?", [env_id, &format!("%\"credential\":\"{id}\"%")], |r| Ok((r.get(0)?, r.get(1)?)))?,
        None => vec![],
    };
    on.push((path.to_string(), key.to_string()));
    for (p, k) in on {
        let hit: Option<i64> = c
            .query_row(
                "SELECT 1 FROM leak_rotations WHERE env_id = ? AND path = ? AND key = ? AND credential <> ? AND state IN ('claimed', 'rotated') AND at > ? LIMIT 1",
                params![env_id, p, k, credential, now - CAP_MS],
                |r| r.get(0),
            )
            .optional()?;
        if hit.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A folder's leaked keys, and its leaked credentials, each with a key it was found by.
type Leaked<'a> = (BTreeSet<String>, BTreeMap<&'a str, &'a str>);

fn warn_unrecorded(e: Error) -> HashMap<String, LeakRotate> {
    tracing::warn!("recording a leak failed: {e}");
    HashMap::new()
}

/// A shared managed link's credential, from its row (`owner`, `link`, `link_state`): what a leak
/// of it rotates, and what a rotation request names as leaked. One without a credential id (from
/// before they had one) is named by its folder and key.
pub(crate) fn credential_of(owner: &str, link: Option<&str>, state: Option<&str>, path: &str, key: &str) -> Option<String> {
    if !owner.is_empty() {
        return None;
    }
    let link: crate::Link = serde_json::from_str(link?).ok()?;
    if link.mode != crate::Mode::Managed {
        return None;
    }
    let state: Json = serde_json::from_str(state?).ok()?;
    Some(state.get("credential").and_then(Json::as_str).map_or_else(|| format!("key:{path}:{key}"), |c| format!("cred:{c}")))
}
