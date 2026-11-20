//! Who a request is (docs/secrets.md, "Access"): a roll-access grant (`rlt_…` tokens, OAuth
//! connections, workload identities' logins), a signed-in person, or the operator. Looked up on
//! every request, never cached.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, names};
use code_sql::OptionalExtension;
use std::sync::Arc;

/// A rule's environments, folders and actions, checked: at least one action, real slugs and
/// folders, and never more than `caller` has.
pub(crate) type Scoped = (Option<Vec<String>>, Option<Vec<String>>, Vec<Action>);

pub(crate) fn scope(project: &str, environments: Option<Vec<String>>, paths: Option<Vec<String>>, actions: Vec<Action>, caller: &Caller) -> Result<Scoped> {
    if actions.is_empty() {
        return Err(Error::BadRequest("at least one action".into()));
    }
    let mut actions = actions;
    actions.sort();
    actions.dedup();
    let paths = paths.map(|ps| ps.iter().map(|p| names::path(p)).collect::<Result<Vec<_>>>()).transpose()?;
    if let Some(envs) = &environments {
        for e in envs {
            names::slug(e)?;
        }
    }
    for a in &actions {
        match &environments {
            Some(envs) => {
                for e in envs {
                    caller.need(*a, &Scope::env(project, e))?;
                }
            }
            None => caller.need(*a, &Scope::project(project))?,
        }
    }
    Ok((environments, paths, actions))
}

/// Client addresses checked and written as CIDRs (an address alone is its /32 or /128); at most 20.
pub(crate) fn allowed(list: Option<Vec<String>>) -> Result<Option<Vec<String>>> {
    let Some(list) = list else { return Ok(None) };
    if list.is_empty() || list.len() > 20 {
        return Err(Error::BadRequest("allowed_ips lists 1–20 addresses or CIDRs (leave it out for any)".into()));
    }
    list.iter()
        .map(|a| {
            let a = a.trim();
            a.parse::<ipnet::IpNet>().or_else(|_| a.parse::<std::net::IpAddr>().map(ipnet::IpNet::from)).map(|n| n.trunc().to_string()).map_err(|_| Error::BadRequest(format!("{a} isn't an address or a CIDR")))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Whether `ip` is in `list` (no address known: not allowed).
pub(crate) fn ip_allowed(list: &[String], ip: Option<&str>) -> bool {
    let Some(ip) = ip.and_then(|i| i.parse::<std::net::IpAddr>().ok()) else { return false };
    list.iter().filter_map(|n| n.parse::<ipnet::IpNet>().ok()).any(|n| n.contains(&ip))
}

/// Who made something, in the end ([`Secrets::maker`]).
pub(crate) enum Maker {
    /// A person (`usr_…`): it acts while they can sign in and reach the project.
    Person(String),
    /// The operator (or a token of theirs).
    Operator,
    /// A link of the chain is gone.
    Gone,
}

impl Secrets {
    /// Who a request is: a roll-access grant (as its person, when it's one's own token), a
    /// signed-in person, or the operator (or anyone when rolld has no operator token).
    pub fn caller(&self, parts: &axum::http::request::Parts) -> Result<Caller> {
        let mut caller = self.caller_of(roll_auth::Operator::on(&parts.extensions), parts.extensions.get::<roll_auth::SignedIn>(), roll_auth::Grant::on(&parts.extensions))?;
        caller.ip = self.client_ip(parts);
        // Only with a grant: the gate sets it beside the grant whose request an approval let through.
        if roll_auth::Grant::on(&parts.extensions).is_some() {
            caller.approval = roll_auth::Approved::on(&parts.extensions).map(crate::access::approval);
        }
        Ok(caller)
    }

    /// Who someone is from what rolld's gate put on their request: a roll-access grant (a person's
    /// own acting as them), a signed-in person (an admin is the operator), or the operator (or
    /// anyone when rolld has no operator token). Another product asking on someone's behalf (roll
    /// apps' `may_reveal`) gets the same answer a request to secrets would.
    pub fn caller_of(&self, operator: bool, user: Option<&roll_auth::SignedIn>, grant: Option<&roll_auth::Grant>) -> Result<Caller> {
        // A roll-access grant first: the gate may put the operator marker beside it, narrowed by
        // its rules, so the marker alone would read as everything.
        if let Some(g) = grant {
            let grants = self.grant_grants(g)?;
            // A person's session (docs/members.md): the person, by their memberships' rules.
            if g.kind == roll_auth::GrantKind::Session {
                let user = g.user.as_deref().or(user.map(|u| u.user.as_str())).unwrap_or_default();
                return Ok(Caller::person(user, grants));
            }
            // A workload token an identity's login got is that identity in the audit log, and
            // reads as one: never past a freeze, whatever its rules.
            return Ok(match self.login_of(&g.id)? {
                Some(identity) => Caller::acting(format!("identity:{identity}"), grants),
                None => Caller::token(&g.id, grants).of_person(g.user.as_deref()),
            });
        }
        // A signed-in person without a grant beside them (an in-process call on their behalf): their
        // session's grant as roll-access builds it now. An admin's session is the operator, as their
        // token would be.
        if let Some(u) = user.filter(|u| !u.admin) {
            return self.person_caller(&u.user, false);
        }
        if self.open || operator || user.is_some() {
            return Ok(Caller::operator());
        }
        Err(Error::Unauthorized("roll secrets needs the operator token, a roll token (rlt_…) or a session".into()))
    }

    /// What a roll-access grant may do here: its rules, and a person's own token only within their
    /// session's grant, whatever its rules name (it acts as them). An admin's reaches what its rules
    /// say, as the admin's session does.
    pub(crate) fn grant_grants(&self, g: &roll_auth::Grant) -> Result<Arc<dyn crate::access::Grants>> {
        let grant = crate::access::GrantGrants(g.clone(), self.protected());
        Ok(match (&g.user, g.kind) {
            (_, roll_auth::GrantKind::Session) => Arc::new(grant),
            (Some(user), _) if !self.person(user)?.is_some_and(|p| p.admin) => Arc::new(crate::access::Within(grant, self.session_grants(user))),
            _ => Arc::new(grant),
        })
    }

    /// `user`'s session grant now: their memberships' roles (docs/members.md), as roll-access builds
    /// it. Nothing when roll-access can't tell (no grants here, or they can't sign in).
    pub(crate) fn session_grants(&self, user: &str) -> crate::access::GrantGrants {
        let g = self.grants.as_ref().and_then(|g| g.session(user)).unwrap_or_else(|| roll_auth::Grant {
            id: format!("session:{user}").into(),
            name: "session".into(),
            kind: roll_auth::GrantKind::Session,
            scope: Arc::new(roll_auth::scope::Scope(vec![])),
            user: Some(user.into()),
            ask_otherwise: false,
        });
        crate::access::GrantGrants(g, self.protected())
    }

    /// Which environments are protected (a protection policy), for rules that leave them out.
    pub(crate) fn protected(&self) -> crate::access::Protected {
        let db = self.db.clone();
        crate::access::Protected::of(move |project, env| {
            db.read(|c| c.query_row("SELECT protected FROM environments WHERE project_id = ? AND slug = ?", [project, env], |r| r.get::<_, i64>(0)).optional()).ok().flatten().map(|p| p != 0)
        })
    }

    /// The client's address: as rolld's gate saw it (`roll_auth::ClientIp`, which requests made on
    /// a caller's behalf, such as MCP's, carry on), else the peer's, or behind a proxy rolld trusts
    /// the last X-Forwarded-For entry: the one that proxy appended. Entries before it are the
    /// client's own to write (ALB, Cloudflare and nginx append to what the client sent), so a pinned
    /// address or a read alert never takes them.
    pub(crate) fn client_ip(&self, parts: &axum::http::request::Parts) -> Option<String> {
        if let Some(roll_auth::ClientIp(ip)) = parts.extensions.get::<roll_auth::ClientIp>() {
            return Some(ip.to_string());
        }
        let forwarded = || {
            parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit(',').next()).map(|s| s.trim().chars().take(64).collect::<String>()).filter(|s| !s.is_empty())
        };
        let peer = || parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string());
        if self.trust_forwarded_for { forwarded().or_else(peer) } else { peer() }
    }

    /// A person as a caller: an admin is the operator; anyone else, their session's grant now (their
    /// memberships' roles), named by their user id.
    pub(crate) fn person_caller(&self, user: &str, admin: bool) -> Result<Caller> {
        if admin {
            return Ok(Caller::operator());
        }
        Ok(Caller::person(user, Arc::new(self.session_grants(user))))
    }

    /// `user`'s account as it is now: `None` once it's gone or disabled (or nothing tells).
    pub(crate) fn person(&self, user: &str) -> Result<Option<crate::access::Person>> {
        match &self.people {
            Some(p) => p.person(user).map_err(|e| Error::Internal(format!("looking up a person's account: {e}"))),
            None => Ok(None),
        }
    }

    /// Whether `project`'s secrets may reach repository `repo` (`owner/name`) through roll code
    /// (its checks and previews, leak blockers, missing-key notices): the workspace's one rule
    /// ([`roll_workspace::Workspace::trusts`]).
    pub fn counts_for_repo(&self, project: &str, repo: &str) -> bool {
        self.workspace.get(roll_workspace::Scope::All, project).is_ok_and(|p| self.workspace.trusts(&p, roll_workspace::Kind::Repo, repo))
    }

    /// Whether what `by` made (an identity, a backup schedule, a gateway, an ACME binding) may still
    /// do each of `needs` (the action and the scope the made thing exercises, nothing broader):
    /// walked back to whoever made the first link of the chain. A person must still be able to sign
    /// in and their session's grant must allow every one of them now (an admin account always, the
    /// operator always). Asked on every use, so removing or demoting someone stops what they made,
    /// or narrows it, at once.
    pub(crate) fn maker_reaches(&self, by: &str, needs: &[(Action, Scope<'_>)]) -> Result<bool> {
        match self.maker(by)? {
            Maker::Gone => Ok(false),
            Maker::Operator => Ok(true),
            Maker::Person(user) => {
                let Some(p) = self.person(&user)? else { return Ok(false) };
                if p.admin {
                    return Ok(true);
                }
                let session = self.session_grants(&user);
                Ok(!needs.is_empty() && needs.iter().all(|(a, at)| crate::access::Grants::allows(&session, *a, at)))
            }
        }
    }

    /// As [`Secrets::maker_reaches`], for something registered (a rotator, an outside database, a
    /// sink, a notify URL…): a person's token that registered it stands for its person even once
    /// the token has ended (rows from before `Caller::maker_id` named the token), so it acts while
    /// that person may, never past them.
    pub(crate) fn registration_reaches(&self, by: &str, needs: &[(Action, Scope<'_>)]) -> Result<bool> {
        if let (Some(id), Some(grants)) = (by.strip_prefix("token:"), &self.grants)
            && let Some(Some(user)) = grants.owner(id)
        {
            return self.maker_reaches(&user, needs);
        }
        self.maker_reaches(by, needs)
    }

    /// The exports (`(env_id, path, to_project)`) matching `filter` whose maker may no longer manage
    /// the folder they share: they count for nothing (`crate::exports::exported`). An export is
    /// bound to whoever made it (a token: until it ends, or its person leaves), checked on every
    /// read that crosses it.
    pub(crate) fn dead_exports(&self, filter: &str, arg: &str) -> Result<crate::exports::Dead> {
        let rows: Vec<(String, String, String, String, String, String)> = self.db.read(|c| {
            c.query(
                &format!("SELECT x.env_id, x.path, x.to_project, x.created_by, e.project_id, e.slug FROM exports x JOIN environments e ON e.id = x.env_id WHERE {filter}"),
                [arg],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
        })?;
        let mut dead = crate::exports::Dead::default();
        for (env, path, to, by, project, slug) in rows {
            if !self.maker_reaches(&by, &[(Action::Admin, Scope::at(&project, &slug, &path))])? {
                dead.insert((env, path, to));
            }
        }
        Ok(dead)
    }

    /// The provider of `kind`, for `ref`: when `ref` is a project's own record (a rotator, an outside
    /// database, …), only while whoever registered it may still manage the project, as they are
    /// now. A removed or demoted admin's rotator stops choosing values at once. Revoking what one
    /// issued doesn't come through here: ending a credential is always allowed.
    pub(crate) fn provider_for(&self, kind: &str, r#ref: &str) -> Result<&Arc<dyn crate::links::LinkProvider>> {
        let p = self.links.get(kind)?;
        if let Some((project, by)) = p.maker(r#ref)
            && !self.registration_reaches(&by, &[(Action::Admin, Scope::whole(&project))])?
        {
            return Err(Error::Link(format!("{kind} {ref} was registered by {by}, who may no longer manage project {project}: an admin of it registers it again", ref = r#ref)));
        }
        Ok(p)
    }

    /// Who made `by` in the end: an identity's maker, a token's person (a roll-access grant made by
    /// someone acts as them), up the chain. A token that's gone (revoked, expired, its person
    /// unable to sign in) or an identity that's gone ends it.
    pub(crate) fn maker(&self, by: &str) -> Result<Maker> {
        let mut by = by.to_string();
        for _ in 0..16 {
            if crate::access::is_person(&by) {
                return Ok(Maker::Person(by));
            }
            let parent: Option<String> = if let Some(id) = by.strip_prefix("token:") {
                match &self.grants {
                    // roll-access as it answers now: a person's token continues with them.
                    Some(grants) => match grants.grant(id) {
                        None => return Ok(Maker::Gone),
                        Some(g) => match &g.user {
                            Some(user) => Some(user.to_string()),
                            None => return Ok(Maker::Operator),
                        },
                    },
                    None => return Ok(Maker::Operator),
                }
            } else if let Some(i) = by.strip_prefix("identity:") {
                self.db.read(|c| c.query_row("SELECT created_by FROM identities WHERE id = ?", [i], |r| r.get::<_, String>(0)).optional())?
            } else {
                return Ok(Maker::Operator);
            };
            let Some(parent) = parent else { return Ok(Maker::Gone) };
            by = parent;
        }
        Ok(Maker::Gone)
    }

    /// Who the caller is, for `roll secrets` to find its project.
    pub fn whoami(&self, caller: &Caller) -> serde_json::Value {
        serde_json::json!({ "principal": caller.principal, "project": caller.grants.project() })
    }
}

#[cfg(test)]
mod tests {
    use super::{allowed, ip_allowed};

    #[test]
    fn allowed_addresses_are_cidrs_and_match_by_containment() {
        let list = allowed(Some(vec!["203.0.113.7".into(), "10.1.2.3/8".into(), "2001:db8::/32".into()])).unwrap().unwrap();
        assert_eq!(list, ["203.0.113.7/32", "10.0.0.0/8", "2001:db8::/32"]);
        assert!(ip_allowed(&list, Some("10.200.0.1")) && ip_allowed(&list, Some("2001:db8::1")) && ip_allowed(&list, Some("203.0.113.7")));
        assert!(!ip_allowed(&list, Some("203.0.113.8")) && !ip_allowed(&list, None), "no address known: not allowed");
        assert!(allowed(Some(vec!["not-an-ip".into()])).is_err() && allowed(Some(vec![])).is_err());
    }
}
