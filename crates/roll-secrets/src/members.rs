//! A member's rights in a project shrank (docs/members.md, "Remove, and leave"): removed, left,
//! demoted, or their account deleted. What they made that hands values on, and that nothing checks
//! again at its next use, goes now. rolld calls [`Secrets::member_changed`] for each workspace
//! member event, after its write; it's safe to run twice.
//!
//! Everything else a person made checks them on every use instead (`maker_reaches`,
//! `registration_reaches`, `dead_exports`, `Within`, `person_caller`): their tokens, syncs,
//! identities, gateways, backups, sinks, notify URLs, registered providers, and the folders they
//! shared.

use crate::access::{Action, Caller, Grants, Scope, is_person};
use crate::caller::Origin;
use crate::{Result, Secrets, audit};
use code_sql::params;

impl Secrets {
    /// `user` lost rights in roll (docs/accounts.md, "Offboarding"): disabled, no longer an admin,
    /// or deleted. [`Secrets::member_changed`] for every project holding something they made that
    /// nothing checks again (an open share, a live lease, a shared folder, a key bound to hosts),
    /// themselves or through a token or identity of theirs, and for their memberships' projects:
    /// never a project only others made things in. Their sessions' grant sees nothing once they're
    /// disabled, so those end. Safe to run twice; nothing comes back when they're enabled again.
    pub fn person_offboarded(&self, user: &str) -> Result<()> {
        if !is_person(user) {
            return Ok(());
        }
        // Who made each, by project: rows naming them, or naming a token or an identity (whose
        // chain may end with them: `made_by`, asked once for each). Others' own rows and the
        // operator's are never read.
        let made: Vec<(String, String)> = self.db.read(|c| {
            c.query(
                "SELECT project_id, created_by FROM shares WHERE ciphertext IS NOT NULL AND (created_by = ?1 OR created_by LIKE 'token:%' OR created_by LIKE 'identity:%')
                 UNION SELECT project_id, issued_by FROM leases WHERE revoked_at IS NULL AND (issued_by = ?1 OR issued_by LIKE 'token:%' OR issued_by LIKE 'identity:%')
                 UNION SELECT project_id, consumer FROM leases WHERE revoked_at IS NULL AND (consumer = ?1 OR consumer LIKE 'token:%' OR consumer LIKE 'identity:%')
                 UNION SELECT e.project_id, x.created_by FROM exports x JOIN environments e ON e.id = x.env_id WHERE x.created_by = ?1 OR x.created_by LIKE 'token:%' OR x.created_by LIKE 'identity:%'
                 UNION SELECT project_id, set_by FROM key_hosts WHERE set_by = ?1 OR set_by LIKE 'token:%' OR set_by LIKE 'identity:%'",
                [user],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })?;
        let mut theirs: std::collections::HashMap<String, bool> = Default::default();
        let mut projects = std::collections::BTreeSet::new();
        for (project, by) in made {
            let mine = match theirs.get(&by) {
                Some(m) => *m,
                None => {
                    let m = self.made_by(&by, user)?;
                    theirs.insert(by, m);
                    m
                }
            };
            if mine {
                projects.insert(project);
            }
        }
        let memberships = self.workspace.memberships_of(user).map_err(|e| crate::Error::Internal(format!("reading an offboarded person's memberships: {e}")))?;
        projects.extend(memberships.into_iter().map(|m| m.project));
        let mut failed = None;
        for project in projects {
            if let Err(e) = self.member_changed(&project, user) {
                tracing::warn!(%project, "ending an offboarded person's leases and shares: {e}");
                failed = Some(e);
            }
        }
        failed.map_or(Ok(()), Err)
    }

    /// Whether `by` (a person, or a chain of tokens and identities, as roll secrets records who
    /// made or copied something) ends with `user`. A token already gone stands for whoever it
    /// was: its owner, as roll-access still knows it.
    pub fn made_by(&self, by: &str, user: &str) -> Result<bool> {
        if by == user {
            return Ok(true);
        }
        if let (Some(id), Some(grants)) = (by.strip_prefix("token:"), &self.grants)
            && grants.owner(id).flatten().as_deref() == Some(user)
        {
            return Ok(true);
        }
        Ok(matches!(self.maker(by)?, Origin::Person(p) if p == user))
    }

    /// `user`'s rights in `project` are what their session grant says now. Whatever they made,
    /// themselves or through a token of theirs (`token:tok_…` resolves to them through
    /// [`Secrets::maker`]):
    /// - no longer allowed to manage the project: the folders they shared with other projects stop
    ///   being shared (`exports`);
    /// - no longer a member: the one-time shares they made end, and the dynamic leases issued to
    ///   them are revoked.
    pub fn member_changed(&self, project: &str, user: &str) -> Result<()> {
        if !is_person(user) || self.project(project).is_err() {
            return Ok(());
        }
        let member = self.session_grants(user).sees(project);
        let manages = self.maker_reaches(user, &[(Action::Admin, Scope::whole(project))])?;
        if manages && member {
            return Ok(());
        }
        crate::metrics::Counters::add(&self.counters.member_rechecks, 1);
        // A key they bound stops counting for its hosts (`binder_reaches`): the project's consumers'
        // interception CAs are checked at their next ask, and one naming such a host ends.
        self.db.write(|c| c.execute(&format!("{}project_id = ?)", crate::intercept::RECHECK_HOLDERS), [project]))?;
        let theirs = |by: &str| self.made_by(by, user);
        let now = self.db.now_ms()?;
        let roll = Caller::consumer("roll:members");
        if !manages {
            let rows: Vec<(String, String, String, String)> = self.db.read(|c| {
                c.query("SELECT x.env_id, x.path, x.to_project, x.created_by FROM exports x JOIN environments e ON e.id = x.env_id WHERE e.project_id = ?", [project], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
            })?;
            let mut made = vec![];
            for (env, path, to, by) in rows {
                if theirs(&by)? {
                    made.push((env, path, to));
                }
            }
            if !made.is_empty() {
                self.db.write(|c| {
                    for (env, path, to) in &made {
                        c.execute("DELETE FROM exports WHERE env_id = ? AND path = ? AND to_project = ?", params![env, path, to])?;
                    }
                    audit::record(c, self.keys()?, project, &roll, "exports.set", None, None, &[], &format!("{} folder share{} {user} made stopped: they may no longer manage the project", made.len(), if made.len() == 1 { "" } else { "s" }), now)?;
                    Ok::<_, crate::Error>(())
                })?;
            }
        }
        if !member {
            let rows: Vec<(String, String)> = self.db.read(|c| c.query("SELECT id, created_by FROM shares WHERE project_id = ? AND ciphertext IS NOT NULL", [project], |r| Ok((r.get(0)?, r.get(1)?))))?;
            let mut ended = vec![];
            for (id, by) in rows {
                if theirs(&by)? {
                    ended.push(id);
                }
            }
            if !ended.is_empty() {
                self.db.write(|c| {
                    for id in &ended {
                        c.execute("UPDATE shares SET ciphertext = NULL, views_left = 0 WHERE id = ? AND project_id = ?", [id.as_str(), project])?;
                    }
                    audit::record(c, self.keys()?, project, &roll, "share.revoke", None, None, &[], &format!("{} share{} {user} made ended: they left the project", ended.len(), if ended.len() == 1 { "" } else { "s" }), now)?;
                    Ok::<_, crate::Error>(())
                })?;
            }
            let leases: Vec<(String, Option<String>, String)> = self.db.read(|c| c.query("SELECT id, issued_by, consumer FROM leases WHERE project_id = ? AND revoked_at IS NULL", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
            for (id, issued_by, consumer) in leases {
                if !(issued_by.as_deref().map(theirs).transpose()?.unwrap_or(false) || theirs(&consumer)?) {
                    continue;
                }
                // A failed revocation goes back to the sweeper, which tries again.
                if let Err(e) = self.revoke_one(&id) {
                    tracing::warn!(lease = %id, "revoking a removed member's lease: {e}");
                }
            }
        }
        Ok(())
    }
}
