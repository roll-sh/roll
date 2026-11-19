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
            // A workload token an identity's login got is that identity in the audit log, and
            // reads as one: never past a freeze, whatever its rules.
            let grants = self.grant_grants(g)?;
            return Ok(match self.login_of(&g.id)? {
                Some(identity) => Caller::acting(format!("identity:{identity}"), grants),
                None => Caller::token(&g.id, grants),
            });
        }
        // A signed-in person (the gate resolved the session on this request): their own projects.
        // An admin's session is the operator, as their token would be.
        if let Some(u) = user.filter(|u| !u.admin) {
            return self.person_caller(&u.user, false);
        }
        if self.open || operator || user.is_some() {
            return Ok(Caller::operator());
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

    /// A person as a caller: an admin is the operator; anyone else, the projects they own as the
    /// workspace has them now, named by their user id.
    pub(crate) fn person_caller(&self, user: &str, admin: bool) -> Result<Caller> {
        if admin {
            return Ok(Caller::operator());
        }
        Ok(Caller::person(user, Arc::new(self.own_projects(user)?)))
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
    /// reach the project (an admin then, or its owner now); the operator always.
    pub(crate) fn maker_reaches(&self, by: &str, project: &str) -> Result<bool> {
        match self.maker(by)? {
            Maker::Gone => Ok(false),
            Maker::Operator => Ok(true),
            Maker::Person(user) => {
                let Some(p) = self.person(&user)? else { return Ok(false) };
                Ok(p.admin || self.workspace.ids_of(&user).map_err(|e| Error::Internal(e.to_string()))?.iter().any(|x| x == project))
            }
        }
    }

    /// What `by` (the operator, a person or a roll-access token) may do now, under another name:
    /// what it made acts as it (a sync, a secret request's answer). `None` once it can't act: a
    /// person who can't sign in, a token revoked or expired, anything else.
    pub(crate) fn acting_as(&self, by: &str, principal: String) -> Result<Option<Caller>> {
        if by == "operator" {
            return Ok(Some(Caller::acting(principal, Arc::new(crate::access::Everything))));
        }
        // A person's: as they are now, while their account can sign in.
        if crate::access::is_person(by) {
            let Some(person) = self.person(by)? else { return Ok(None) };
            return Ok(Some(self.person_caller(by, person.admin)?.acting_as(principal)));
        }
        // A roll-access grant: as it is now (a person's acts as them).
        let Some(g) = by.strip_prefix("token:").and_then(|t| self.grants.as_ref()?.grant(t)) else { return Ok(None) };
        Ok(Some(Caller::acting(principal, self.grant_grants(&g)?)))
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
