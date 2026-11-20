//! A member's rights in a project shrank (docs/members.md, "Remove, and leave"): removed, left,
//! demoted, or their account deleted. What they made that hands values on, and that nothing checks
//! again at its next use, goes now. rolld calls [`Secrets::member_changed`] for each workspace
//! member event, after its write; it's safe to run twice.
//!
//! Everything else a person made checks them on every use instead (`maker_reaches`, `Within`,
//! `person_caller`): their tokens, syncs, identities, gateways, backups, sinks, rotators and
//! notify URLs.

use crate::access::{Action, Caller, Grants, Scope, is_person};
use crate::{Result, Secrets, audit};
use code_sql::params;

impl Secrets {
    /// `user`'s rights in `project` are what their session grant says now:
    /// - no longer allowed to manage the project: the folders they shared with other projects stop
    ///   being shared (`exports` they made);
    /// - no longer a member: the one-time shares they made end, and the dynamic leases issued to
    ///   them are revoked.
    pub fn member_changed(&self, project: &str, user: &str) -> Result<()> {
        if !is_person(user) || self.project(project).is_err() {
            return Ok(());
        }
        let session = self.session_grants(user);
        let member = session.sees(project);
        let manages = self.maker_reaches(user, &[(Action::Admin, Scope::whole(project))])?;
        let now = self.db.now_ms()?;
        let roll = Caller::consumer("roll:members");
        if !manages {
            self.db.write(|c| {
                let made: Vec<(String, String, String)> = c.query(
                    "SELECT x.env_id, x.path, x.to_project FROM exports x JOIN environments e ON e.id = x.env_id WHERE e.project_id = ? AND x.created_by = ?",
                    [project, user],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                for (env, path, to) in &made {
                    c.execute("DELETE FROM exports WHERE env_id = ? AND path = ? AND to_project = ?", params![env, path, to])?;
                }
                if !made.is_empty() {
                    audit::record(c, self.keys()?, project, &roll, "exports.set", None, None, &[], &format!("{} folder share{} {user} made stopped: they may no longer manage the project", made.len(), if made.len() == 1 { "" } else { "s" }), now)?;
                }
                Ok::<_, crate::Error>(())
            })?;
        }
        if !member {
            self.db.write(|c| {
                let n = c.execute("UPDATE shares SET ciphertext = NULL, views_left = 0 WHERE project_id = ? AND created_by = ? AND ciphertext IS NOT NULL", [project, user])?;
                if n > 0 {
                    audit::record(c, self.keys()?, project, &roll, "share.revoke", None, None, &[], &format!("{n} share{} {user} made ended: they left the project", if n == 1 { "" } else { "s" }), now)?;
                }
                Ok::<_, crate::Error>(())
            })?;
            let leases: Vec<String> = self.db.read(|c| c.query("SELECT id FROM leases WHERE project_id = ? AND revoked_at IS NULL AND (issued_by = ? OR consumer = ?)", [project, user, user], |r| r.get(0)))?;
            for id in leases {
                // A failed revocation goes back to the sweeper, which tries again.
                if let Err(e) = self.revoke_one(&id) {
                    tracing::warn!(lease = %id, "revoking a removed member's lease: {e}");
                }
            }
        }
        Ok(())
    }
}
