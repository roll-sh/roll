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

impl Secrets {
    /// Who a request is: a roll-access grant (as its person, when it's one's own token), a
    /// signed-in person, or the operator (or anyone when rolld has no operator token).
    pub fn caller(&self, parts: &axum::http::request::Parts) -> Result<Caller> {
        let ip = self.client_ip(parts);
        // A roll-access grant first: the gate may put the operator marker beside it, narrowed by
        // its rules, so the marker alone would read as everything.
        if let Some(g) = roll_auth::Grant::on(&parts.extensions) {
            // A workload token an identity's login got is that identity in the audit log.
            let principal = match self.login_of(&g.id)? {
                Some(identity) => format!("identity:{identity}"),
                None => format!("token:{}", g.id),
            };
            return Ok(Caller { principal, grants: self.grant_grants(g)?, ip });
        }
        // A signed-in person (the gate resolved the session on this request): their own projects.
        // An admin's session is the operator, as their token would be.
        if let Some(u) = parts.extensions.get::<roll_auth::SignedIn>().filter(|u| !u.admin) {
            let mut caller = self.person_caller(&u.user, false)?;
            caller.ip = ip;
            return Ok(caller);
        }
        if self.open || roll_auth::Operator::on(&parts.extensions) {
            let mut c = Caller::operator();
            c.ip = ip;
            return Ok(c);
        }
        Err(Error::Unauthorized("roll secrets needs the operator token, a roll token (rlt_…) or a session".into()))
    }

    /// What a roll-access grant may do here: its rules, and a person's own token only on their own
    /// projects, whatever its rules name (it acts as them). An admin's reaches what its rules say,
    /// as the admin's session does.
    pub(crate) fn grant_grants(&self, g: &roll_auth::Grant) -> Result<Arc<dyn crate::access::Grants>> {
        let grant = crate::access::GrantGrants(g.clone());
        Ok(match &g.user {
            Some(user) if !self.person(user)?.is_some_and(|p| p.admin) => Arc::new(crate::access::Within(grant, self.own_projects(user)?)),
            _ => Arc::new(grant),
        })
    }

    /// The peer's address, or the first X-Forwarded-For one behind a proxy rolld trusts.
    pub(crate) fn client_ip(&self, parts: &axum::http::request::Parts) -> Option<String> {
        let forwarded = || {
            parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|s| s.trim().chars().take(64).collect::<String>()).filter(|s| !s.is_empty())
        };
        let peer = || parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string());
        if self.trust_forwarded_for { forwarded().or_else(peer) } else { peer() }
    }

    /// A person as a caller: an admin is the operator; anyone else, the projects they own as the
    /// workspace has them now, named by their user id.
    pub(crate) fn person_caller(&self, user: &str, admin: bool) -> Result<Caller> {
        if admin {
            return Ok(Caller::operator());
        }
        Ok(Caller { principal: user.to_string(), grants: Arc::new(self.own_projects(user)?), ip: None })
    }

    /// The projects `user` owns, as the workspace has them now, and their roll code owner.
    pub(crate) fn own_projects(&self, user: &str) -> Result<crate::access::OwnProjects> {
        let projects = self.workspace.ids_of(user).map_err(|e| Error::Internal(e.to_string()))?.into_iter().collect();
        Ok(crate::access::OwnProjects { projects, code_owner: self.person(user)?.and_then(|p| p.code_owner) })
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

    /// Whether what `by` made (an identity, a sync) may still act on `project`: walked back to
    /// whoever made the first link of the chain. A person must still be able to sign in and still
    /// reach the project (an admin then, or its owner now); the operator and roll-access's grants
    /// (roll-access answers for those) always.
    pub(crate) fn maker_reaches(&self, by: &str, project: &str) -> Result<bool> {
        let mut by = by.to_string();
        for _ in 0..16 {
            if crate::access::is_person(&by) {
                let Some(p) = self.person(&by)? else { return Ok(false) };
                return Ok(p.admin || self.workspace.ids_of(&by).map_err(|e| Error::Internal(e.to_string()))?.iter().any(|x| x == project));
            }
            let parent: Option<String> = if by.starts_with("token:") {
                // A roll-access grant: roll-access answers for it (and a person's acts as them).
                return Ok(true);
            } else if let Some(i) = by.strip_prefix("identity:") {
                self.db.read(|c| c.query_row("SELECT created_by FROM identities WHERE id = ?", [i], |r| r.get::<_, String>(0)).optional())?
            } else {
                return Ok(true);
            };
            let Some(parent) = parent else { return Ok(false) };
            by = parent;
        }
        Ok(false)
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
