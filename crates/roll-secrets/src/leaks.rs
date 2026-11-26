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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
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

/// The most candidates one scan takes (`POST …/leaks`, `POST …/scan`).
pub const SCAN_MAX: usize = 20_000;

/// The shortest candidate a scanner's check (`POST …/scan`) tries.
pub const SCAN_MIN_LEN: usize = 16;
/// A scanner's calls a minute per caller; candidates it tries a minute, per caller and per
/// project; and an hour per project (a push is at most [`SCAN_MAX`], and a hook runs a few times
/// a minute).
pub const SCANS_PER_MINUTE: i64 = 60;
pub const SCAN_CANDIDATES_PER_MINUTE: i64 = 20_000;
pub const SCAN_CANDIDATES_PER_HOUR: i64 = 200_000;
/// The most refs, and the longest pusher and ref, a hook's call names.
const PUSH_REFS_MAX: usize = 100;
const PUSHER_MAX: usize = 100;
const REF_MAX: usize = 256;
/// The most entries in a project's `breakglass` list.
const BREAKGLASS_MAX: usize = 100;

/// Whether a scanner's check tries `c`: `c` and each part of it that someone who knows its shape
/// wouldn't know ([`unknown_parts`]: a URL's password, the right side of `user:…`) are
/// [`strong`] on their own. A match is a candidate equal to a whole value, so this is the value's
/// own floor: `postgres://app:<guess>@db:5432/x` is never tried for a guess at a weak password,
/// however long the known text around it.
pub fn scannable(c: &str) -> bool {
    strong(c) && unknown_parts(c).into_iter().all(strong)
}

/// The parts of `c` someone who knows its shape wouldn't know: a URL's password (or its user,
/// with no password); with no user, its query's secret-named values (`key`, `token`, `sig`…; all
/// of them when none is) or else its path's last segment; the values of a `k=v;k=v` or `k=v&k=v`
/// list (its secret-named ones when there are any); the right side of `name:value` or
/// `name=value` (`user:pass@host` its password); and each word of text with spaces. Anything else
/// is itself.
fn unknown_parts(c: &str) -> Vec<&str> {
    /// The values of `pairs` whose names say they're secret, or all of them when none does.
    fn named<'a>(pairs: Vec<(&'a str, &'a str)>) -> Vec<&'a str> {
        let secret_named = |k: &str| {
            let k = k.to_ascii_lowercase();
            ["pass", "pwd", "secret", "token", "key", "sig", "auth", "cred", "code"].iter().any(|w| k.contains(w))
        };
        let secret: Vec<&str> = pairs.iter().filter(|(k, _)| secret_named(k)).map(|(_, v)| *v).collect();
        if secret.is_empty() { pairs.into_iter().map(|(_, v)| v).collect() } else { secret }
    }
    if c.contains(char::is_whitespace) {
        return c.split_whitespace().flat_map(unknown_parts).collect();
    }
    if let Some((scheme, rest)) = c.split_once("://")
        && !scheme.is_empty()
        && scheme.chars().all(|x| x.is_ascii_alphanumeric() || "+-.".contains(x))
    {
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            return vec![userinfo.split_once(':').map_or(userinfo, |(_, p)| p)];
        }
        let tail = tail.split('#').next().unwrap_or("");
        let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
        if !query.is_empty() {
            return named(query.split('&').map(|kv| kv.split_once('=').unwrap_or(("", kv))).collect());
        }
        return vec![path.split('/').rfind(|s| !s.is_empty()).unwrap_or(c)];
    }
    // base64's padding isn't a `name=value`.
    let bare = c.trim_end_matches('=');
    if bare.contains([';', '&']) && bare.contains('=') {
        return named(bare.split([';', '&']).filter(|f| !f.is_empty()).map(|f| f.split_once('=').unwrap_or(("", f))).collect()).into_iter().flat_map(unknown_parts).collect();
    }
    if let Some(i) = bare.find([':', '=']) {
        let name = &bare[..i];
        if !name.is_empty() && name.len() <= 64 && name.chars().all(|x| x.is_ascii_alphanumeric() || "_-.".contains(x)) {
            let value = &c[i + 1..];
            return unknown_parts(value.rsplit_once('@').map_or(value, |(v, _)| v));
        }
    }
    vec![c]
}

/// At least [`SCAN_MIN_LEN`] characters, two kinds of character, and 3 bits of entropy a
/// character (`aaaaaaaaaaaaaaaa1` and a word repeated aren't).
fn strong(c: &str) -> bool {
    let n = c.chars().count();
    if !(SCAN_MIN_LEN..=4096).contains(&n) {
        return false;
    }
    let kinds = [c.chars().any(|x| x.is_ascii_lowercase()), c.chars().any(|x| x.is_ascii_uppercase()), c.chars().any(|x| x.is_ascii_digit()), c.chars().any(|x| !x.is_ascii_alphanumeric())];
    if kinds.iter().filter(|k| **k).count() < 2 {
        return false;
    }
    let mut counts: HashMap<char, usize> = HashMap::new();
    for x in c.chars() {
        *counts.entry(x).or_default() += 1;
    }
    let bits: f64 = counts
        .values()
        .map(|&k| {
            let p = k as f64 / n as f64;
            -p * p.log2()
        })
        .sum();
    bits >= 3.0
}

/// One more call by `who` in `minute`, of `n` candidates: (calls, candidates) in that minute so far.
/// The one counter `/scan` (by principal) and a backup verify (`backup.verify:<principal>`) share;
/// inside a write, as each caller's read-then-write is one.
fn bump(c: &code_sql::Conn<'_>, who: &str, minute: i64, n: i64) -> Result<(i64, i64)> {
    Ok(c.query_row(
        "INSERT INTO scan_counts (principal, minute, calls, candidates) VALUES (?, ?, 1, ?) ON CONFLICT (principal, minute) DO UPDATE SET calls = scan_counts.calls + 1, candidates = scan_counts.candidates + excluded.candidates RETURNING calls, candidates",
        params![who, minute, n],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// A scanner's calls per caller and minute (`POST …/scan`'s limit).
pub(crate) const SCAN_COUNTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scan_counts (
    principal TEXT NOT NULL,
    minute INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    candidates INTEGER NOT NULL,
    PRIMARY KEY (principal, minute)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS scan_project_counts (
    project_id TEXT NOT NULL,
    minute INTEGER NOT NULL,
    candidates INTEGER NOT NULL,
    PRIMARY KEY (project_id, minute)
) WITHOUT ROWID;
";

/// What a pre-receive hook says about the push it checks (`POST …/scan`): who pushed (as the git
/// server tells the hook: `GITHUB_USER_LOGIN`, `GL_USERNAME`…) and to which refs. Text for the
/// audit log; the pusher counts for nothing but the project's `breakglass` list.
#[derive(Clone, Debug, Default)]
pub struct Push {
    /// The push goes through anyway (`git push -o breakglass`), if the project allows it.
    pub breakglass: bool,
    pub pusher: Option<String>,
    pub refs: Vec<String>,
}

impl Push {
    fn check(&self) -> Result<()> {
        let plain = |s: &str, max: usize| !s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control);
        if self.pusher.as_deref().is_some_and(|p| !plain(p, PUSHER_MAX)) {
            return Err(Error::BadRequest(format!("pusher is 1 to {PUSHER_MAX} printable characters")));
        }
        if self.refs.len() > PUSH_REFS_MAX || self.refs.iter().any(|r| !plain(r, REF_MAX)) {
            return Err(Error::BadRequest(format!("at most {PUSH_REFS_MAX} refs, each 1 to {REF_MAX} printable characters")));
        }
        Ok(())
    }

    /// ` by alice to refs/heads/main, refs/heads/x`, for a summary.
    fn said(&self) -> String {
        let by = self.pusher.as_deref().map(|p| format!(" by {p}")).unwrap_or_default();
        let to = if self.refs.is_empty() { String::new() } else { format!(" to {}", self.refs.join(", ")) };
        format!("{by}{to}")
    }
}

/// A scanner's answer (`POST …/scan`): which candidate, and the environment of the value it is;
/// the key and folder only where the caller may also `read` (a scan-only token names no key).
#[derive(Clone, Debug, Serialize)]
pub struct ScanFound {
    pub index: usize,
    pub environment: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

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

    /// A scanner's check (`POST …/scan`, the `scan` right: a pre-receive hook): which whole
    /// `candidates` are values of this project, in the environments and folders the caller may
    /// `scan`, without `read` or `reveal`. Only [`scannable`] candidates are tried, so it can't
    /// confirm a guess at a short or simple value, nor at a weak password inside text a guesser
    /// knows; sensitive values and personal ones never match; [`SCANS_PER_MINUTE`] calls and
    /// [`SCAN_CANDIDATES_PER_MINUTE`] tried candidates a minute per caller, and as many a minute
    /// and [`SCAN_CANDIDATES_PER_HOUR`] an hour per project. A match names its key and folder only
    /// where the caller may `read`. Every call is audited (`secrets.scan`, with the pusher and refs
    /// the hook names and the keys found), never a candidate. A `breakglass` push goes through
    /// anyway, audited as `secrets.scan.breakglass`, and only when the project's `breakglass`
    /// list allows the hook's token or the pusher ([`Secrets::breakglass`]): 403 otherwise.
    pub fn scan_only(&self, project: &str, candidates: &[String], caller: &Caller, push: &Push) -> Result<Vec<ScanFound>> {
        caller.need(Action::Scan, &Scope::project(project))?;
        if candidates.len() > SCAN_MAX {
            return Err(Error::BadRequest(format!("at most {SCAN_MAX} candidates a scan")));
        }
        push.check()?;
        self.project(project)?;
        let action = if push.breakglass { "secrets.scan.breakglass" } else { "secrets.scan" };
        let said = push.said();
        // Only what's worth a scanner's question: a short or simple candidate, or one whose
        // unknown part is, is never tried.
        let tried: Vec<String> = candidates.iter().map(|c| if scannable(c) { c.clone() } else { String::new() }).collect();
        let n = tried.iter().filter(|c| !c.is_empty()).count() as i64;
        if caller.principal != "operator" {
            let minute = self.db.now_ms()? / 60_000;
            let (calls, mine, project_minute, project_hour): (i64, i64, i64, i64) = self.db.write(|c| {
                let (calls, mine) = bump(c, &caller.principal, minute, n)?;
                let at = c.query_row(
                    "INSERT INTO scan_project_counts (project_id, minute, candidates) VALUES (?, ?, ?) ON CONFLICT (project_id, minute) DO UPDATE SET candidates = scan_project_counts.candidates + excluded.candidates RETURNING candidates",
                    params![project, minute, n],
                    |r| r.get(0),
                )?;
                let hour = c.query_row("SELECT COALESCE(SUM(candidates), 0) FROM scan_project_counts WHERE project_id = ? AND minute > ?", params![project, minute - 60], |r| r.get(0))?;
                Ok::<_, Error>((calls, mine, at, hour))
            })?;
            if calls > SCANS_PER_MINUTE || mine > SCAN_CANDIDATES_PER_MINUTE || project_minute > SCAN_CANDIDATES_PER_MINUTE || project_hour > SCAN_CANDIDATES_PER_HOUR {
                let limits = format!("{SCANS_PER_MINUTE} scans and {SCAN_CANDIDATES_PER_MINUTE} candidates tried a minute per token, {SCAN_CANDIDATES_PER_MINUTE} a minute and {SCAN_CANDIDATES_PER_HOUR} an hour per project");
                self.audit_now(project, caller, action, None, None, &[], &format!("refused a scan of {} candidates{said}: over {limits}", candidates.len()));
                return Err(Error::TooMany(format!("at most {limits}: try again later")));
            }
        }
        if push.breakglass
            && let Err(e) = self.floors()?.breakglass()
        {
            self.audit_now(project, caller, action, None, None, &[], &format!("refused break glass{said}: roll secrets' policy allows none"));
            return Err(e);
        }
        if push.breakglass && !self.breakglass(project)?.iter().any(|w| allows_breakglass(w, caller, push)) {
            self.audit_now(project, caller, action, None, None, &[], &format!("refused break glass{said}: the project's breakglass list doesn't name this token or pusher"));
            return Err(Error::Forbidden(format!("break glass isn't allowed{said} in this project: an admin names who may with `roll secrets breakglass`")));
        }
        let found: Vec<Found> = self
            .matches(project, &tried, Some(&caller.principal), false)?
            .into_iter()
            .filter(|m| !m.leak.environment.contains('/') && caller.allows(Action::Scan, &Scope::at(project, &m.leak.environment, &m.leak.path)))
            .map(|m| Found { index: m.index, leak: m.leak })
            .collect();
        let summary = format!("scanned {} candidates ({n} tried){said}: {} matched{}", candidates.len(), found.len(), if push.breakglass { ", let through with breakglass" } else { "" });
        if found.is_empty() {
            self.audit_now(project, caller, action, None, None, &[], &summary);
        }
        let mut at: BTreeMap<(&str, &str), BTreeSet<String>> = BTreeMap::new();
        for f in &found {
            at.entry((f.leak.environment.as_str(), f.leak.path.as_str())).or_default().insert(f.leak.key.clone());
        }
        for ((env, path), keys) in at {
            let keys: Vec<String> = keys.into_iter().collect();
            let event = Event::new(Kind::ScanFinding, json!({ "candidates": candidates.len(), "matched": found.len(), "guard": false, "breakglass": push.breakglass, "pusher": push.pusher }));
            self.audit_now_event(project, caller, action, Some(env), Some(path), &keys, &summary, Some(event));
        }
        Ok(found
            .into_iter()
            .map(|f| {
                let named = caller.allows(Action::Read, &Scope::at(project, &f.leak.environment, &f.leak.path));
                ScanFound { index: f.index, key: named.then(|| f.leak.key.clone()), path: named.then(|| f.leak.path.clone()), environment: f.leak.environment }
            })
            .collect())
    }

    /// Who may push past `project`'s pre-receive hooks (`git push -o breakglass`): pushers' logins
    /// as the git server names them, `token:<id>` for every push through one hook's token, `*`
    /// for anyone. Empty (the default): no one.
    pub fn breakglass(&self, project: &str) -> Result<Vec<String>> {
        let v: Option<String> = self.db.read(|c| c.query_row("SELECT breakglass FROM projects WHERE project_id = ?", [project], |r| r.get::<_, Option<String>>(0)).optional())?.flatten();
        Ok(v.map(|v| crate::from_json(&v)).unwrap_or_default())
    }

    /// Set [`Secrets::breakglass`] (an admin's; audited as `scan.breakglass`).
    pub fn set_breakglass(&self, project: &str, who: Vec<String>, caller: &Caller) -> Result<Vec<String>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if !breakglass_ok(&who) {
            return Err(Error::BadRequest(format!("breakglass is at most {BREAKGLASS_MAX} of: a pusher's login, token:<id>, or *")));
        }
        let mut who = who;
        who.sort();
        who.dedup();
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if !who.is_empty() {
                crate::guardrails::read(c)?.breakglass()?;
            }
            c.execute("UPDATE projects SET breakglass = ? WHERE project_id = ?", params![(!who.is_empty()).then(|| json!(who).to_string()), project])?;
            let summary = if who.is_empty() { "no one may push past a pre-receive hook".to_string() } else { format!("may push past a pre-receive hook: {}", who.join(", ")) };
            crate::audit::record(c, self.keys()?, project, caller, "scan.breakglass", None, None, &[], &summary, now)
        })?;
        Ok(who)
    }

    /// One more call by `who` this minute, outside a scan (a backup verify counts as
    /// `backup.verify:<principal>`): (calls, candidates) so far this minute, in `scan_counts` as
    /// [`bump`] keeps it for `/scan`.
    pub(crate) fn count_calls(&self, who: &str, n: i64) -> Result<(i64, i64)> {
        let minute = self.db.now_ms()? / 60_000;
        self.db.write(|c| bump(c, who, minute, n))
    }

    /// Minutes of scan counts that have passed (the sweeper): a caller's last minute, a project's
    /// last hour.
    pub(crate) fn forget_scan_counts(&self) -> Result<()> {
        let minute = self.db.now_ms()? / 60_000;
        self.db.write(|c| {
            c.execute("DELETE FROM scan_counts WHERE minute < ?", [minute - 1])?;
            c.execute("DELETE FROM scan_project_counts WHERE minute <= ?", [minute - 60])
        })?;
        Ok(())
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
            crate::guardrails::read(c)?.leaks(rotate)?;
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
                    // A public certificate's key: its certificate is revoked at the CA first.
                    let mut revoking = vec![];
                    for (env_id, _, key) in &now_rotating {
                        let row: Option<(Option<String>, Option<String>)> = c.query_row("SELECT link, link_state FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ''", [env_id.as_str(), path, key.as_str()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                        if let Some((_, serial)) = row.and_then(|(l, s)| crate::public_certs::held_serial(l.as_deref(), s.as_deref())) {
                            revoking.push(serial);
                        }
                    }
                    if !revoking.is_empty() {
                        summary = format!("{summary}; its certificate {} is revoked at the CA first (keyCompromise)", revoking.join(", "));
                        data["revoking"] = json!(revoking);
                    }
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

/// Whether `who` is a `breakglass` list roll takes: at most [`BREAKGLASS_MAX`] of `*`,
/// `token:<id>` and pushers' logins.
pub(crate) fn breakglass_ok(who: &[String]) -> bool {
    let login = |w: &str| (1..=PUSHER_MAX).contains(&w.len()) && w.chars().all(|x| x.is_ascii_alphanumeric() || "._-@+".contains(x));
    who.len() <= BREAKGLASS_MAX && who.iter().all(|w| w == "*" || w.strip_prefix("token:").map_or_else(|| login(w), login))
}

/// Whether `entry` of a project's `breakglass` list lets `push` through: `token:<id>` only the
/// hook's own token, `*` anyone, anything else the pusher's login (which can't name a token).
fn allows_breakglass(entry: &str, caller: &Caller, push: &Push) -> bool {
    match entry {
        "*" => true,
        e if e.starts_with("token:") => e == caller.principal,
        e => push.pusher.as_deref() == Some(e),
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
