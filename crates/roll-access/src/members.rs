//! Project members as grants (docs/members.md): a person's session is a grant of their
//! memberships' rules (`roll_auth::roles`), built on every request from the workspace and never
//! cached; and what a membership change does to their tokens and the audit log.

use crate::store::AuditEvent;
use crate::{Access, Error, Result, new_id};
use std::sync::Arc;

impl Access {
    /// `user`'s session grant now: the rules of every project they're a member of. `session`: the
    /// session's id (`ses_…`), else it's named for the person.
    pub fn session_grant(&self, user: &str, session: Option<&str>) -> Result<roll_auth::Grant> {
        let memberships = match self.workspace.get() {
            Some(ws) => ws.memberships_of(user).map_err(|e| Error::Internal(format!("reading memberships: {e}")))?,
            None => Vec::new(),
        };
        let id = session.map_or_else(|| format!("session:{user}"), str::to_string);
        Ok(roll_auth::Grant { id: id.into(), name: "session".into(), kind: roll_auth::GrantKind::Session, scope: Arc::new(roll_auth::roles::session_scope(&memberships)), user: Some(user.into()), ask_otherwise: false })
    }

    /// A membership change, after its write (rolld runs this for every `MemberEvent`): it goes in
    /// roll's audit log, and a removal revokes the person's tokens confined to the project (D7),
    /// one audit row each, the removal named in why. Their other tokens only stop reaching the
    /// project, as their session does.
    pub fn member_event(&self, e: &roll_workspace::MemberEvent) -> Result<()> {
        self.store.audit(&AuditEvent {
            id: new_id("aud"),
            at: e.at,
            grant_id: None,
            grant_name: None,
            source: "workspace".into(),
            tool: None,
            method: String::new(),
            route: String::new(),
            product: Some("workspace".into()),
            action: Some(e.action.clone()),
            resource: Some(e.project.clone()),
            decision: "allow".into(),
            approval: None,
            status: 200,
            args: Some(serde_json::json!({ "event": e.id, "actor": e.actor, "subject": e.subject, "detail": e.detail })),
            ip: e.ip.clone(),
            duration_ms: 0,
            client: None,
        });
        let Some(user) = e.removal() else { return Ok(()) };
        if !roll_auth::roles::decisions::REVOKE_CONFINED_ON_REMOVE {
            return Ok(());
        }
        let now = code_sql::now_ms();
        let confined: Vec<String> = self
            .store
            .list()?
            .into_iter()
            .filter(|g| g.user.as_deref() == Some(user) && g.live(now) && !g.decoy)
            .filter(|g| roll_auth::roles::confined_to(&g.rules, g.ask_otherwise, &e.project))
            .map(|g| g.id)
            .collect();
        let why = format!("removed from project {} ({})", e.project, e.id);
        for ids in confined.chunks(crate::store::MAX_REVOKE) {
            for g in self.store.revoke_many(ids, &why)? {
                self.store.audit(&AuditEvent {
                    id: new_id("aud"),
                    at: now,
                    grant_id: Some(g.id.clone()),
                    grant_name: Some(g.name.clone()),
                    source: "workspace".into(),
                    tool: None,
                    method: String::new(),
                    route: String::new(),
                    product: Some("access".into()),
                    action: Some("admin".into()),
                    resource: Some(why.clone()),
                    decision: "revoke".into(),
                    approval: None,
                    status: 200,
                    args: None,
                    ip: None,
                    duration_ms: 0,
                    client: None,
                });
            }
        }
        Ok(())
    }
}
