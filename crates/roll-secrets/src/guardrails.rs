//! Guardrails (docs/secrets.md, "Guardrails"): one server-wide policy of floors every project may
//! only tighten, as `.roll/review.yaml` only tightens a repo's settings. The operator or an admin
//! account sets it (`PUT /v1/secrets/policy`); each write it covers reads it in its own
//! transaction and answers 403 naming the floor when it would go below it. A new project's
//! `production` and `leaks.rotate` are born at the floors; existing projects aren't rewritten, and
//! Health lists how far each is from them (`off_policy`).
//!
//! The token lifetime floor is enforced by roll-access, which makes every token: rolld hands it
//! [`Secrets::token_days_cap`] for grants with a secrets rule.

use crate::access::Caller;
use crate::leaks::LeakRotate;
use crate::{Error, Result, Secrets};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS guardrails (
    id INTEGER PRIMARY KEY,
    policy TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL
);
";

/// The floors. Absent or null: no floor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guardrails {
    /// Every project's `production` stays protected with at least this many approvals (0–10).
    #[serde(default)]
    pub production_approvals: Option<i64>,
    /// `"managed"`: every project's `leaks.rotate` stays `managed`.
    #[serde(default)]
    pub leaks_rotate: Option<LeakRotate>,
    /// The only sync kinds a project may make (those made through roll secrets; a product's own
    /// syncs, such as roll search's, aren't covered).
    #[serde(default)]
    pub sync_kinds: Option<Vec<String>>,
    /// No sync signs in to AWS, Google or Azure with a key stored in roll where it could sign in
    /// without one (the project's AWS role, workload identity federation).
    #[serde(default)]
    pub no_stored_cloud_keys: bool,
    /// The longest a roll-access token with a secrets rule may live, in days (1–3650).
    #[serde(default)]
    pub max_token_days: Option<u32>,
    /// No project lets a push past its pre-receive hooks (`breakglass` stays empty).
    #[serde(default)]
    pub no_breakglass: bool,
}

/// `GET`/`PUT /v1/secrets/policy`'s answer.
#[derive(Debug, Serialize)]
pub struct Policy {
    #[serde(flatten)]
    pub floors: Guardrails,
    pub updated_at: Option<i64>,
    pub updated_by: Option<String>,
}

/// A roll-access grant with a secrets rule that outlives `max_token_days` (made before it).
#[derive(Debug, Serialize)]
pub struct Outliving {
    pub id: String,
    pub name: String,
    /// `token` (machine clients' too) or `oauth` (an MCP client's or an enterprise app's connection).
    pub kind: String,
    /// Whose it is (`None`: the operator's).
    pub user: Option<String>,
    pub created_at: i64,
    /// `None`: it never ends.
    pub expires_at: Option<i64>,
}

/// `GET /v1/secrets/policy/report`: every exception to the policy, across projects.
#[derive(Debug, Serialize)]
pub struct Report {
    pub policy: Policy,
    /// Each project below a floor, with its Health's `off_policy` findings.
    pub projects: Vec<crate::health::ProjectFindings>,
    /// Grants with a secrets rule that live past `max_token_days` from now, or never end.
    pub grants: Vec<Outliving>,
}

/// The 403 for a write below a floor.
pub(crate) fn below(floor: &str) -> Error {
    Error::Forbidden(refusal(floor))
}

fn refusal(floor: &str) -> String {
    format!("roll secrets' policy (GET /v1/secrets/policy): {floor}; a project may only tighten it")
}

/// The policy as stored (no row: no floors).
pub(crate) fn read(c: &Conn<'_>) -> Result<Guardrails> {
    Ok(stored(c)?.map(|(g, _, _)| g).unwrap_or_default())
}

fn stored(c: &Conn<'_>) -> Result<Option<(Guardrails, i64, String)>> {
    let row = c.query_row("SELECT policy, updated_at, updated_by FROM guardrails WHERE id = 1", (), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))).optional()?;
    Ok(row.map(|(p, at, by)| (serde_json::from_str(&p).unwrap_or_default(), at, by)))
}

fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl Guardrails {
    /// `production`'s floor, said as the refusal says it.
    fn production_floor(n: i64) -> String {
        format!("production stays protected with at least {n} approval{}", plural(n))
    }

    /// A policy for `production` (`set_policy`, a rename onto it): refused below the floor.
    pub(crate) fn production(&self, protected: bool, approvals: i64) -> Result<()> {
        match self.production_approvals {
            Some(n) if !protected || approvals < n => Err(below(&Self::production_floor(n))),
            _ => Ok(()),
        }
    }

    /// Whether `production` may stop being one (deleted, or renamed away): not under a floor.
    pub(crate) fn keeps_production(&self) -> Result<()> {
        match self.production_approvals {
            Some(n) => Err(below(&format!("{}, so it isn't deleted or renamed", Self::production_floor(n)))),
            None => Ok(()),
        }
    }

    /// A new `production`'s approvals, protected (`None`: as roll makes it without a policy).
    pub(crate) fn born_approvals(&self) -> Option<i64> {
        self.production_approvals
    }

    pub(crate) fn leaks(&self, rotate: LeakRotate) -> Result<()> {
        if self.leaks_rotate == Some(LeakRotate::Managed) && rotate != LeakRotate::Managed {
            return Err(below("leaks.rotate stays managed"));
        }
        Ok(())
    }

    /// A project's `breakglass` list naming anyone, or a push asking to break glass: refused under
    /// `no_breakglass`.
    pub(crate) fn breakglass(&self) -> Result<()> {
        if self.no_breakglass {
            return Err(below("no project lets a push past its pre-receive hook (no_breakglass)"));
        }
        Ok(())
    }

    /// A new sync of `kind` (made through roll secrets); `stored_cloud_key`: what it should use
    /// instead of the cloud key it would store, if it would.
    pub(crate) fn sync(&self, kind: &str, stored_cloud_key: Option<&str>) -> Result<()> {
        if let Some(kinds) = &self.sync_kinds
            && !kinds.iter().any(|k| k == kind)
        {
            let allowed = if kinds.is_empty() { "none".to_string() } else { kinds.join(", ") };
            return Err(below(&format!("{kind} syncs aren't among the kinds allowed ({allowed})")));
        }
        if let (true, Some(instead)) = (self.no_stored_cloud_keys, stored_cloud_key) {
            return Err(below(&format!("no sync stores a cloud key for AWS, Google or Azure: {instead}")));
        }
        Ok(())
    }

    /// A machine identity's credentials of `hours` (a new bootstrap chain): within `max_token_days`.
    pub(crate) fn machine_hours(&self, hours: u32) -> Result<()> {
        match self.max_token_days {
            Some(d) if hours > d * 24 => Err(below(&format!("a token with a secrets rule lives at most {d} day{}, so a machine credential at most {} hours", plural(d.into()), d * 24))),
            _ => Ok(()),
        }
    }

    /// The hours of a machine credential issued now: a chain made before the floor comes back
    /// within it.
    pub(crate) fn cap_hours(&self, hours: u32) -> u32 {
        self.max_token_days.map_or(hours, |d| hours.min(d * 24))
    }

    fn summary(&self) -> String {
        let mut floors = vec![];
        if let Some(n) = self.production_approvals {
            floors.push(Self::production_floor(n));
        }
        if self.leaks_rotate == Some(LeakRotate::Managed) {
            floors.push("leaks.rotate managed".into());
        }
        if let Some(k) = &self.sync_kinds {
            floors.push(format!("sync kinds {}", if k.is_empty() { "none".into() } else { k.join(", ") }));
        }
        if self.no_stored_cloud_keys {
            floors.push("no stored cloud keys".into());
        }
        if let Some(d) = self.max_token_days {
            floors.push(format!("tokens at most {d} days"));
        }
        if self.no_breakglass {
            floors.push("no break glass".into());
        }
        if floors.is_empty() { "roll secrets' policy: no floors".into() } else { format!("roll secrets' policy: {}", floors.join("; ")) }
    }
}

impl Secrets {
    fn operator_only(caller: &Caller) -> Result<()> {
        if !caller.grants.everything() {
            return Err(Error::Forbidden("roll secrets' policy is the operator's, or an admin account's".into()));
        }
        Ok(())
    }

    /// The policy (the operator's, or an admin account's).
    pub fn guardrails(&self, caller: &Caller) -> Result<Policy> {
        Self::operator_only(caller)?;
        let row = self.db.read(stored)?;
        Ok(match row {
            Some((floors, at, by)) => Policy { floors, updated_at: Some(at), updated_by: Some(by) },
            None => Policy { floors: Guardrails::default(), updated_at: None, updated_by: None },
        })
    }

    /// Replace the policy (the operator's, or an admin account's). Nothing existing is rewritten:
    /// the floors apply to the next write each covers. Audited in every project, as roll's
    /// issuer key's rotation is.
    pub fn set_guardrails(&self, mut g: Guardrails, caller: &Caller) -> Result<Policy> {
        Self::operator_only(caller)?;
        if g.production_approvals.is_some_and(|n| !(0..=10).contains(&n)) {
            return Err(Error::BadRequest("production_approvals is 0–10".into()));
        }
        if g.leaks_rotate == Some(LeakRotate::Off) {
            return Err(Error::BadRequest("leaks_rotate is \"managed\" or null (no floor)".into()));
        }
        if g.max_token_days.is_some_and(|d| !(1..=3650).contains(&d)) {
            return Err(Error::BadRequest("max_token_days is 1–3650".into()));
        }
        if let Some(kinds) = &mut g.sync_kinds {
            let offered: Vec<&str> = self.syncs.kinds().filter(|t| !t.internal()).map(|t| t.kind()).collect();
            if let Some(k) = kinds.iter().find(|k| !offered.contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("{k} isn't a sync kind made through roll secrets here ({})", offered.join(", "))));
            }
            kinds.sort();
            kinds.dedup();
        }
        let keys = self.keys()?;
        let now = self.db.now_ms()?;
        let by = caller.made_by().to_string();
        let summary = g.summary();
        self.db.write(|c| {
            c.execute(
                "INSERT INTO guardrails (id, policy, updated_at, updated_by) VALUES (1, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET policy = excluded.policy, updated_at = excluded.updated_at, updated_by = excluded.updated_by",
                params![crate::json(&g), now, by],
            )?;
            for project in c.query("SELECT project_id FROM projects WHERE forgotten_at IS NULL", (), |r| r.get::<_, String>(0))? {
                crate::audit::record(c, keys, &project, caller, "policy.set", None, None, &[], &summary, now)?;
            }
            Ok::<_, Error>(())
        })?;
        tracing::info!(by = %by, "{summary}");
        Ok(Policy { floors: g, updated_at: Some(now), updated_by: Some(by) })
    }

    /// Every exception to the policy (the operator's, or an admin account's): each project below
    /// a floor, as its Health says (`off_policy`), and the grants made before `max_token_days`
    /// that outlive it. Projects aren't rewritten when the policy changes; this is the list.
    pub fn policy_report(&self, caller: &Caller) -> Result<Report> {
        let policy = self.guardrails(caller)?;
        let projects: Vec<String> = self.db.read(|c| c.query("SELECT project_id FROM projects WHERE forgotten_at IS NULL ORDER BY project_id", (), |r| r.get(0)))?;
        let mut below = vec![];
        for project in projects {
            let envs: Vec<crate::envs::EnvRow> = self.db.read(|c| crate::envs::all(c, &project))?.into_iter().filter(|e| e.kind == crate::envs::Kind::Standard).collect();
            let findings = self.off_policy(&project, caller, &policy.floors, &envs)?;
            if findings.is_empty() {
                continue;
            }
            let project_name = self.workspace.get(roll_workspace::Scope::All, &project).map(|p| p.name).unwrap_or_else(|_| project.clone());
            below.push(crate::health::ProjectFindings { project, project_name, findings });
        }
        let grants = match (policy.floors.max_token_days, &self.workloads) {
            (Some(d), Some(w)) => w.outliving(d)?,
            _ => vec![],
        };
        Ok(Report { policy, projects: below, grants })
    }

    /// The longest a roll-access token with a secrets rule may live, in days, and the refusal's
    /// words (rolld hands it to roll-access, which makes and changes every token).
    pub fn token_days_cap(&self) -> Result<Option<(u32, String)>> {
        let g = self.db.read(read)?;
        Ok(g.max_token_days.map(|d| (d, refusal(&format!("a token with a secrets rule lives at most {d} day{}", plural(d.into()))))))
    }

    /// The policy, for Health's distance (`off_policy`).
    pub(crate) fn floors(&self) -> Result<Guardrails> {
        self.db.read(read)
    }
}
