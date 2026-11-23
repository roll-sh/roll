//! Project members, invites and homes (docs/members.md): who shares a project, with which role,
//! and which project a resource was made through.
//!
//! A membership is person × project × role (`roll_auth::roles`). This module stores them and
//! enforces what only a write can (one membership per person and project, the last owner, an
//! invite used once). What a role may do is roll-access's: the gate decides every route from the
//! session's grant, and here a change of role is checked only against the caller's grant
//! ([`Actor::may_grant`]: nobody grants more than they hold).
//!
//! Every change records a [`MemberEvent`] in the same transaction (`member_events`, the outbox):
//! rolld writes it to roll's audit log and acts on removals (D7's revocations) from there, after
//! the write, so a crash between the two loses nothing.

use crate::events::{self, Kind as Event};
use crate::{Error, Kind, Result, Scope, Workspace, not_found, owner_of};
use code_sql::{Conn, OptionalExtension, params};
use roll_auth::roles::{self, Role};
use roll_auth::scope::Scope as Rules;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS members (
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL,
    environments TEXT,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, user_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS members_user ON members (user_id);
CREATE TABLE IF NOT EXISTS project_invites (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    role TEXT NOT NULL,
    environments TEXT,
    state TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_by TEXT,
    used_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS project_invites_project ON project_invites (project_id);
CREATE INDEX IF NOT EXISTS project_invites_email ON project_invites (email);
CREATE TABLE IF NOT EXISTS homes (
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    project_id TEXT NOT NULL,
    set_by TEXT NOT NULL,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (kind, ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS homes_project ON homes (project_id);
CREATE TABLE IF NOT EXISTS deleted_members (
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL,
    environments TEXT,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, user_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deleted_homes (
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    project_id TEXT NOT NULL,
    set_by TEXT NOT NULL,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, kind, ref)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS member_events (
    id TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    action TEXT NOT NULL,
    project_id TEXT NOT NULL,
    actor TEXT NOT NULL,
    subject TEXT,
    detail TEXT NOT NULL,
    ip TEXT,
    claimed_at INTEGER,
    done INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS member_events_due ON member_events (done, at);
";

/// The most members a project has.
pub const MEMBERS_MAX: i64 = 500;
/// The most projects a person is a member of.
pub const PROJECTS_MAX: i64 = 1_000;
/// How long an invite's link works.
pub const INVITE_TTL_MS: i64 = 7 * 24 * 3_600_000;
/// The most pending invites a project has.
pub const INVITES_MAX: i64 = 200;

/// The kinds a project can be the home of: those whose refs are random ids a product never gives
/// out again (`prj_…`, `rdb_…`, `st_…`, `srch_…`, NoSQL's, `svc_…`). A bucket, a queues vhost, a
/// tasks tenant or a repo goes by a name that can be deleted and made again by someone else, and a
/// home kept by name would hand the new one the old one's trust; until those products tell the
/// workspace when they delete one, they get no home (their trust comes from `Owners` alone).
pub const HOME_KINDS: [Kind; 6] = [Kind::Postgres, Kind::Redis, Kind::Stream, Kind::Nosql, Kind::Search, Kind::Service];

/// Who people are, as roll accounts has them now (rolld answers; never cached). `None`: no such
/// account, or it's disabled.
pub trait People: Send + Sync {
    fn person(&self, user: &str) -> Option<Person>;
    /// Tell `email` about an invite (`text` holds the link). Best effort.
    fn mail(&self, email: &str, subject: &str, text: &str) {
        let _ = (email, subject, text);
    }
    /// Whether SSO connection `connection`'s SCIM groups are on now (its provider keeps its
    /// groups, so roll's routes don't delete them).
    fn scim_groups_on(&self, connection: &str) -> bool {
        let _ = connection;
        false
    }
    /// Whether SSO connection `connection` is gone for sure (a failed lookup is a no): its groups
    /// are then dropped by [`Workspace::sweep_groups`].
    fn connection_gone(&self, connection: &str) -> bool {
        let _ = connection;
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub handle: String,
    pub email: Option<String>,
    /// One of roll's admin accounts: the operator in every project (D9).
    pub admin: bool,
}

/// Run after a resource gets a home ([`Workspace::on_homed`]): `(kind, ref, project)`.
pub type OnHomed = Box<dyn Fn(Kind, &str, &str) + Send + Sync>;

/// What [`Workspace::trusts`] reads of a project itself: its namespace (empty: the operator's)
/// and its owners ([`Workspace::trust_basis`]).
#[derive(Clone, Debug)]
pub struct TrustBasis {
    namespace: String,
    owners: Vec<String>,
}

/// Who is changing membership, and what they hold: the operator (no grant: everything), or a
/// grant's rules (a person's session, their token and the session it acts within, an operator's
/// token).
#[derive(Clone, Debug, Default)]
pub struct Actor {
    /// `usr_…`, `operator`, or `token:tok_…` for an operator's token.
    pub id: String,
    /// The person, when it's one (not an admin acting as the operator).
    pub user: Option<String>,
    /// The rules of the request's grant (`None`: the operator, everything).
    pub grant: Option<Rules>,
    /// A person's token acts within their session too.
    pub session: Option<Rules>,
    pub ip: Option<String>,
}

impl Actor {
    pub fn operator() -> Actor {
        Actor { id: "operator".into(), ..Default::default() }
    }

    /// Whether this actor manages `project`'s members: `admin` on the project itself. rolld's gate
    /// decided it already; checked again here, where nothing else stands in front.
    pub fn manages(&self, project: &str) -> bool {
        use roll_auth::scope::{Action, Outcome, Resource, Target};
        let r = Target::One(Resource { project: Some(project.into()), ..Resource::new("workspace", project) });
        let ok = |s: &Option<Rules>| s.as_ref().is_none_or(|s| s.outcome(&r, Action::Admin) == Outcome::Allow);
        // A person with no grant at all (no gate in front) manages nothing.
        (self.user.is_none() || self.grant.is_some()) && ok(&self.grant) && ok(&self.session)
    }

    /// Whether this actor may give or take away `role` in `project`: they manage it, and the role's
    /// rules are covered by theirs (`Rule::covers`), so nobody grants more than they hold.
    pub fn may_grant(&self, project: &str, role: Role, environments: Option<&[String]>) -> bool {
        let ok = |s: &Option<Rules>| s.as_ref().is_none_or(|s| roles::may_grant(s, project, role, environments));
        self.manages(project) && ok(&self.grant) && ok(&self.session)
    }

    pub(crate) fn scope(&self) -> Scope<'_> {
        match &self.user {
            Some(u) => Scope::Member(u),
            None => Scope::All,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Member {
    pub project: String,
    pub user: String,
    pub handle: String,
    /// Shown only to who may manage the project's members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub role: Role,
    pub environments: Option<Vec<String>>,
    pub added_by: String,
    pub added_at: i64,
    /// Their account can't sign in now (disabled or gone): they count for nothing, the last owner
    /// rule included.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inactive: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Invite {
    pub id: String,
    pub project: String,
    pub email: String,
    pub role: Role,
    pub environments: Option<Vec<String>>,
    pub state: String,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// The project's name, for the person it's for.
    pub project_name: String,
}

/// One change to who is in a project, as roll's audit log records it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MemberEvent {
    pub id: String,
    pub at: i64,
    /// `member.invite`, `member.invite_revoke`, `member.accept`, `member.decline`, `member.role`,
    /// `member.remove`, `member.leave`, `home.set`; a group's `group.create`, `group.update`,
    /// `group.delete`, `group.member_add`, `group.member_remove`, `group.role` and
    /// `group.role_remove` (a server-wide one has no project); and `group.lost`, one per person
    /// and project a group change may have narrowed or cut off (`subject`), made from the group's
    /// event after its write.
    pub action: String,
    pub project: String,
    pub actor: String,
    pub subject: Option<String>,
    /// The old and new role and environments, an invite's email and role, a home's resource.
    pub detail: Value,
    pub ip: Option<String>,
}

impl MemberEvent {
    /// The projects a `group.lost` names (where its person no longer holds what the group gave).
    pub fn lost_projects(&self) -> Vec<String> {
        self.detail.get("projects").and_then(|p| serde_json::from_value(p.clone()).ok()).unwrap_or_default()
    }

    /// The person who left the project, when they were removed or left.
    pub fn removal(&self) -> Option<&str> {
        matches!(self.action.as_str(), "member.remove" | "member.leave").then_some(self.subject.as_deref()).flatten()
    }
}

/// Acts on each event after its write (rolld: the audit log, removals' revocations). An error
/// leaves the event to be tried again.
pub type OnEvent = Box<dyn Fn(&MemberEvent) -> std::result::Result<(), String> + Send + Sync>;

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub(crate) fn random(n: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect()
}

pub(crate) fn check_envs(role: Role, environments: &Option<Vec<String>>) -> Result<()> {
    let Some(envs) = environments else { return Ok(()) };
    if role != Role::Developer {
        return Err(Error::BadRequest("only a developer's role is narrowed to environments".into()));
    }
    let ok = |e: &str| (1..=64).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'*') && !e.starts_with('*') && !e[..e.len() - 1].contains('*');
    if envs.is_empty() || envs.len() > 32 || !envs.iter().all(|e| ok(e)) {
        return Err(Error::BadRequest("environments lists 1–32 environment names (a-z, 0-9, -, or a prefix ending in *)".into()));
    }
    Ok(())
}

fn parse_role(s: &str) -> Result<Role> {
    Role::parse(s).ok_or_else(|| Error::BadRequest(format!("unknown role {s:?}: one of viewer, developer, admin, owner")))
}

pub(crate) fn envs_json(e: &Option<Vec<String>>) -> Option<String> {
    e.as_ref().map(|e| serde_json::to_string(e).expect("JSON"))
}

pub(crate) fn envs_of(s: Option<String>) -> Option<Vec<String>> {
    s.and_then(|s| serde_json::from_str(&s).ok())
}

fn normal_email(e: &str) -> Result<String> {
    let e = e.trim().to_ascii_lowercase();
    let ok = e.len() <= 254 && e.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')) && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok { Ok(e) } else { Err(Error::BadRequest("email isn't an email address".into())) }
}

pub(crate) fn record(c: &Conn<'_>, now: i64, action: &str, project: &str, actor: &Actor, subject: Option<&str>, detail: Value) -> Result<()> {
    c.execute(
        "INSERT INTO member_events (id, at, action, project_id, actor, subject, detail, ip) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![format!("mev_{}", random(16)), now, action, project, actor.id, subject, detail.to_string(), actor.ip],
    )?;
    Ok(())
}

/// A membership row: `(user, role, environments, added_by, added_at)`.
type Row = (String, Role, Option<Vec<String>>, String, i64);

fn member_rows(c: &Conn<'_>, project: &str) -> Result<Vec<Row>> {
    Ok(c.query("SELECT user_id, role, environments, added_by, added_at FROM members WHERE project_id = ? ORDER BY added_at, user_id", [project], |r| {
        let role: String = r.get(1)?;
        Ok((r.get(0)?, Role::parse(&role).unwrap_or(Role::Viewer), envs_of(r.get(2)?), r.get(3)?, r.get(4)?))
    })?)
}

fn role_of(c: &Conn<'_>, project: &str, user: &str) -> Result<Option<(Role, Option<Vec<String>>)>> {
    Ok(c.query_row("SELECT role, environments FROM members WHERE project_id = ? AND user_id = ?", [project, user], |r| {
        let role: String = r.get(0)?;
        Ok((Role::parse(&role).unwrap_or(Role::Viewer), envs_of(r.get(1)?)))
    })
    .optional()?)
}

/// 409 `last_owner` unless `project` keeps an owner who can sign in once `leaving` isn't one.
/// `signing`: the owners who could sign in, looked up before the write (a person's account is
/// another database's: a write closure only touches its own).
fn keeps_an_owner(c: &Conn<'_>, project: &str, leaving: &str, signing: &HashSet<String>) -> Result<()> {
    // An operator's project has the operator: it never runs out of owners.
    let namespace: Option<String> = c.query_row("SELECT owner FROM projects WHERE id = ?", [project], |r| r.get(0)).optional()?;
    if namespace.as_deref() == Some("") {
        return Ok(());
    }
    let owners: Vec<String> = c.query("SELECT user_id FROM members WHERE project_id = ? AND role = 'owner'", [project], |r| r.get(0))?;
    if owners.iter().any(|o| o != leaving && signing.contains(o)) {
        return Ok(());
    }
    Err(Error::LastOwner("the project would have no owner who can sign in: make someone else an owner first".into()))
}

impl Workspace {
    /// Who people are (set once, by rolld).
    pub fn set_people(&self, people: std::sync::Arc<dyn People>) {
        let _ = self.people.set(people);
    }

    /// roll-access's grants, for an invite made with a token (set once, by rolld).
    pub fn set_grants(&self, grants: std::sync::Arc<dyn roll_auth::Grants>) {
        let _ = self.grants.set(grants);
    }

    /// Who made an invite (`created_by`), as they are now: the operator; a person by their account
    /// and memberships (an admin account is the operator, D9); a token by its grant now, and a
    /// person's token within their session too. `None`: gone, disabled or revoked.
    fn inviter(&self, by: &str) -> Option<Actor> {
        let as_person = |user: &str, grant: Option<Rules>| -> Option<Actor> {
            let p = self.person(user)?;
            if p.admin && roles::decisions::ADMIN_IS_OPERATOR {
                return Some(Actor { id: by.into(), grant, ..Default::default() });
            }
            let session = roles::session_scope(&self.memberships_of(user).ok()?);
            Some(match grant {
                Some(g) => Actor { id: by.into(), user: Some(user.into()), grant: Some(g), session: Some(session), ip: None },
                None => Actor { id: by.into(), user: Some(user.into()), grant: Some(session), session: None, ip: None },
            })
        };
        if by == "operator" {
            return Some(Actor::operator());
        }
        if let Some(id) = by.strip_prefix("token:") {
            let g = self.grants.get()?.grant(id)?;
            return match g.user.as_deref() {
                Some(u) => as_person(u, Some((*g.scope).clone())),
                None => Some(Actor { id: by.into(), grant: Some((*g.scope).clone()), ..Default::default() }),
            };
        }
        if by.starts_with("usr_") {
            return as_person(by, None);
        }
        None
    }

    /// The dashboard's address, for invite links (set once, by rolld).
    pub fn set_web_url(&self, url: &str) {
        let _ = self.web_url.set(url.trim_end_matches('/').to_string());
    }

    /// Run `f` for every member event after its write (and again until it succeeds).
    pub fn on_event(&self, f: OnEvent) {
        self.on_event.write().expect("on_event").push(f);
    }

    pub(crate) fn person(&self, user: &str) -> Option<Person> {
        self.people.get().and_then(|p| p.person(user))
    }

    /// `user`'s handle as the members list shows it (their id when they can't sign in), looked up
    /// before a write: an account is another database's.
    pub(crate) fn handle(&self, user: &str) -> String {
        self.person(user).map_or_else(|| user.to_string(), |p| p.handle)
    }

    /// The owners of `project` who can sign in now.
    fn signing_owners(&self, project: &str) -> Result<HashSet<String>> {
        Ok(self.owners_of(project)?.into_iter().filter(|o| self.person(o).is_some()).collect())
    }

    /// `user`'s memberships, for their session's grant: their own and their groups' roles
    /// (docs/members-v1-1.md, "Roles add up"), in one indexed read, never cached. Their own come
    /// first, then group roles by project, up to [`PROJECTS_MAX`] distinct projects (only SCIM
    /// can put someone past it; roll's own writes refuse to).
    pub fn memberships_of(&self, user: &str) -> Result<Vec<roles::Membership>> {
        let rows: Vec<(String, String, Option<String>, bool)> = self.db.read(|c| {
            c.query(
                "SELECT project_id, role, environments, 0 AS via FROM members WHERE user_id = ?1 \
                 UNION ALL SELECT gr.project_id, gr.role, gr.environments, 1 AS via FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gm.user_id = ?1 \
                 ORDER BY via, project_id",
                [user],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? == 1)),
            )
        })?;
        let mut projects: HashSet<String> = HashSet::new();
        let mut capped = false;
        let mut out = Vec::with_capacity(rows.len());
        for (project, role, environments, via_group) in rows {
            if !projects.contains(&project) {
                if projects.len() as i64 >= PROJECTS_MAX {
                    capped = true;
                    continue;
                }
                projects.insert(project.clone());
            }
            let role = if via_group { crate::groups::group_role(&role) } else { Role::parse(&role).unwrap_or(Role::Viewer) };
            out.push(roles::Membership { project, role, environments: envs_of(environments) });
        }
        if capped {
            // Only SCIM can put someone here (roll's own writes refuse): never silent.
            tracing::warn!(user = %user, code = "too_many_projects", max = PROJECTS_MAX, "workspace: a person's groups reach more projects than a grant holds; those past the cap are left out");
        }
        Ok(out)
    }

    /// `user`'s highest role in `project`, through their membership or a group (what lists show).
    pub fn role_in(&self, project: &str, user: &str) -> Result<Option<Role>> {
        self.db.read(|c| crate::groups::effective_role(c, project, user))
    }

    /// `user`'s own membership's role in `project`, groups aside (the last owner, the member list).
    pub fn direct_role(&self, project: &str, user: &str) -> Result<Option<Role>> {
        Ok(self.db.read(|c| role_of(c, project, user))?.map(|(r, _)| r))
    }

    /// The owners of `project` (`usr_…`), whether or not they can sign in.
    /// A deleted project's members and their roles as they were when it was deleted. Never who may
    /// bring it back (that's asked of grants as they are now): only what a product keeps doing for
    /// it through its recovery window (roll secrets' audit sinks).
    pub fn deleted_members(&self, project: &str) -> Result<Vec<(String, Role)>> {
        Ok(self.db.read(|c| {
            c.query("SELECT user_id, role FROM deleted_members WHERE project_id = ? ORDER BY user_id", [project], |r| {
                let role: String = r.get(1)?;
                Ok((r.get(0)?, Role::parse(&role).unwrap_or(Role::Viewer)))
            })
        })?)
    }

    /// As [`Workspace::trusted_by_members`], from a deleted project's kept homes and owners.
    pub(crate) fn trusted_when_deleted(&self, project: &str, namespace: &str, kind: Kind, r#ref: &str) -> bool {
        let home = self.db.read(|c| c.query_row("SELECT 1 FROM deleted_homes WHERE project_id = ? AND kind = ? AND ref = ?", [project, kind.as_str(), r#ref], |r| r.get::<_, i64>(0)).optional());
        if roles::decisions::HOMES && HOME_KINDS.contains(&kind) && home.ok().flatten().is_some() {
            return true;
        }
        let Some(owners) = self.owners.get() else { return false };
        let kept = self.deleted_members(project).unwrap_or_default();
        kept.iter().filter(|(_, r)| *r == Role::Owner).any(|(o, _)| if o == namespace { owners.trusts(o, kind, r#ref) } else { owners.owns(o, kind, r#ref) })
    }

    pub fn owners_of(&self, project: &str) -> Result<Vec<String>> {
        Ok(self.db.read(|c| c.query("SELECT user_id FROM members WHERE project_id = ? AND role = 'owner' ORDER BY user_id", [project], |r| r.get(0)))?)
    }

    /// `project`'s members. Emails only when `emails` (the caller manages members).
    pub fn members(&self, scope: Scope<'_>, project: &str, emails: bool) -> Result<Vec<Member>> {
        let rows = self.db.read(|c| {
            owner_of(c, scope, project)?;
            member_rows(c, project)
        })?;
        Ok(rows
            .into_iter()
            .map(|(user, role, environments, added_by, added_at)| {
                let p = self.person(&user);
                Member {
                    project: project.into(),
                    handle: p.as_ref().map_or_else(|| user.clone(), |p| p.handle.clone()),
                    email: p.as_ref().and_then(|p| p.email.clone()).filter(|_| emails),
                    inactive: p.is_none(),
                    user,
                    role,
                    environments,
                    added_by,
                    added_at,
                }
            })
            .collect())
    }

    /// Change `user`'s role in `project`. Both the old and the new role must be the actor's to give.
    pub fn set_role(&self, actor: &Actor, project: &str, user: &str, role: &str, environments: Option<Vec<String>>) -> Result<Member> {
        let role = parse_role(role)?;
        check_envs(role, &environments)?;
        let signing = self.signing_owners(project)?;
        let handle = self.handle(user);
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let Some((old, old_envs)) = role_of(c, project, user)? else { return Err(Error::NotFound(format!("{user} isn't a member of {project}"))) };
            if !actor.may_grant(project, old, old_envs.as_deref()) || !actor.may_grant(project, role, environments.as_deref()) {
                return Err(Error::Forbidden(format!("you can't change a {} into a {}: it's more than your own role gives", old.as_str(), role.as_str())));
            }
            if old == Role::Owner && role != Role::Owner {
                keeps_an_owner(c, project, user, &signing)?;
            }
            c.execute("UPDATE members SET role = ?, environments = ? WHERE project_id = ? AND user_id = ?", params![role.as_str(), envs_json(&environments), project, user])?;
            record(c, now, "member.role", project, actor, Some(user), json!({ "old": { "role": old, "environments": old_envs }, "new": { "role": role, "environments": environments } }))?;
            if (old, &old_envs) != (role, &environments) {
                let data = json!({ "user": user, "handle": handle, "from": { "role": old, "environments": old_envs }, "to": { "role": role, "environments": environments } });
                events::emit(c, project, now, Event::MemberRoleChanged, user, &actor.id, data)?;
            }
            revoke_invites_beyond(c, now, project, user, Some((role, environments.clone())), actor)?;
            Ok(())
        })?;
        self.run_events();
        self.members(Scope::All, project, true)?.into_iter().find(|m| m.user == user).ok_or_else(|| not_found(project))
    }

    /// Take `user` out of `project` (the actor must be able to give their role), or, when `user`
    /// is the actor, leave it.
    pub fn remove_member(&self, actor: &Actor, project: &str, user: &str) -> Result<()> {
        let leaving = actor.user.as_deref() == Some(user);
        let signing = self.signing_owners(project)?;
        let handle = self.handle(user);
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let Some((role, envs)) = role_of(c, project, user)? else {
                // In only through groups: leaving is the groups' (their admins or identity provider).
                let via: Vec<String> = c.query("SELECT g.name FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id JOIN groups g ON g.id = gm.group_id WHERE gr.project_id = ? AND gm.user_id = ? ORDER BY g.name", [project, user], |r| r.get(0))?;
                if !via.is_empty() {
                    let who = if leaving { "you're" } else { "they're" };
                    return Err(Error::Conflict(format!("{who} in this project only through the group {}: ask roll's operator or an admin to change the group", via.join(", "))));
                }
                return Err(Error::NotFound(format!("{user} isn't a member of {project}")));
            };
            if !leaving && !actor.may_grant(project, role, envs.as_deref()) {
                return Err(Error::Forbidden(format!("you can't remove a {}: it's more than your own role gives", role.as_str())));
            }
            if role == Role::Owner {
                keeps_an_owner(c, project, user, &signing)?;
            }
            c.execute("DELETE FROM members WHERE project_id = ? AND user_id = ?", [project, user])?;
            revoke_invites_beyond(c, now, project, user, None, actor)?;
            let action = if leaving { "member.leave" } else { "member.remove" };
            record(c, now, action, project, actor, Some(user), json!({ "role": role, "environments": envs }))?;
            let data = json!({ "user": user, "handle": handle, "role": role, "environments": envs, "reason": if leaving { "left" } else { "removed" } });
            events::emit(c, project, now, Event::MemberRemoved, user, &actor.id, data)?;
            Ok(())
        })?;
        self.run_events();
        Ok(())
    }

    /// Invite `email` to `project` as `role`: the link (with its token) is in the answer once, and
    /// mailed. The answer is the same whether or not the address has an account (D5: one without
    /// waits until roll's operator or an admin makes it; accepting takes signing in).
    pub fn invite(&self, actor: &Actor, project: &str, email: &str, role: &str, environments: Option<Vec<String>>) -> Result<(Invite, String)> {
        let role = parse_role(role)?;
        check_envs(role, &environments)?;
        let email = normal_email(email)?;
        if !actor.may_grant(project, role, environments.as_deref()) {
            return Err(Error::Forbidden(format!("you can't invite a {}: it's more than your own role gives", role.as_str())));
        }
        // Nobody invites themselves: a role held through a group would become their own, and
        // outlast the identity provider taking them out of the group.
        if actor.user.as_deref().and_then(|u| self.person(u)).and_then(|p| p.email).is_some_and(|e| e.eq_ignore_ascii_case(&email)) {
            return Err(Error::Forbidden("you can't invite yourself: a project admin who isn't you invites you".into()));
        }
        // Checksummed and prefixed (`rlv_`), so the credential guard and leak scanning know it.
        let token = roll_auth::credentials::new_secret("rlv_");
        let id = format!("pin_{}", random(12));
        let now = self.db.now_ms()?;
        let name = self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let namespace: String = c.query_row("SELECT owner FROM projects WHERE id = ?", [project], |r| r.get(0))?;
            if namespace.is_empty() && !roles::decisions::OPERATOR_PROJECTS_TAKE_MEMBERS {
                return Err(Error::BadRequest("the operator's projects don't take members".into()));
            }
            let pending: i64 = c.query_row("SELECT COUNT(*) FROM project_invites WHERE project_id = ? AND state = 'pending' AND expires_at > ?", params![project, now], |r| r.get(0))?;
            if pending >= INVITES_MAX {
                return Err(Error::Conflict(format!("a project has at most {INVITES_MAX} pending invites")));
            }
            // A pending invite to the same address is replaced (its old link stops working), but
            // only by someone who may give the role it names: else re-inviting would quietly take
            // a higher invite away.
            let replaced: Vec<(String, Option<String>)> = c.query("SELECT role, environments FROM project_invites WHERE project_id = ? AND email = ? AND state = 'pending' AND expires_at > ?", params![project, email, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
            for (r, e) in &replaced {
                if !actor.may_grant(project, parse_role(r)?, envs_of(e.clone()).as_deref()) {
                    return Err(Error::Conflict(format!("{email} has a pending invite as {r}, which is more than your own role gives: someone who may give it revokes it first")));
                }
            }
            let old: Vec<(String, String, Option<String>)> = c.query("SELECT id, role, environments FROM project_invites WHERE project_id = ? AND email = ? AND state = 'pending'", [project, email.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            c.execute("UPDATE project_invites SET state = 'revoked' WHERE project_id = ? AND email = ? AND state = 'pending'", [project, email.as_str()])?;
            for (old, r, e) in old {
                revoked(c, now, project, &old, &r, envs_of(e), "replaced", &actor.id)?;
            }
            c.execute(
                "INSERT INTO project_invites (id, project_id, token_hash, email, role, environments, state, created_by, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)",
                params![id, project, hash_token(&token), email, role.as_str(), envs_json(&environments), actor.id, now, now + INVITE_TTL_MS],
            )?;
            record(c, now, "member.invite", project, actor, None, json!({ "invite": id, "email": email, "role": role, "environments": environments }))?;
            let data = json!({ "invite": id, "email": email, "role": role, "environments": environments, "expires_at": now + INVITE_TTL_MS });
            events::emit(c, project, now, Event::InviteSent, &id, &actor.id, data)?;
            Ok(c.query_row("SELECT name FROM projects WHERE id = ?", [project], |r| r.get::<_, String>(0))?)
        })?;
        self.run_events();
        let url = format!("{}/dashboard/invites?token={token}", self.web_url.get().map_or("", String::as_str));
        if let Some(p) = self.people.get() {
            let text = format!("You're invited to the project {name} on roll as a {}. Sign in and accept it within 7 days:\n\n{url}\n", role.as_str());
            p.mail(&email, &format!("You're invited to {name} on roll"), &text);
        }
        let invite = Invite { id, project: project.into(), email, role, environments, state: "pending".into(), created_by: actor.id.clone(), created_at: now, expires_at: now + INVITE_TTL_MS, project_name: name };
        Ok((invite, url))
    }

    /// `project`'s invites that are still pending, never a token.
    pub fn invites(&self, scope: Scope<'_>, project: &str) -> Result<Vec<Invite>> {
        let now = self.db.now_ms()?;
        self.db.read(|c| {
            owner_of(c, scope, project)?;
            invite_rows(c, "i.project_id = ? AND i.state = 'pending' AND i.expires_at > ?", params![project, now])
        })
    }

    pub fn revoke_invite(&self, actor: &Actor, project: &str, invite: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let found: Option<(String, Option<String>)> = c.query_row("SELECT role, environments FROM project_invites WHERE id = ? AND project_id = ? AND state = 'pending'", [invite, project], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((role, envs)) = found else { return Err(Error::NotFound(format!("no pending invite {invite}"))) };
            if !actor.may_grant(project, parse_role(&role)?, envs_of(envs.clone()).as_deref()) {
                return Err(Error::Forbidden(format!("you can't revoke an invite as {role}: it's more than your own role gives")));
            }
            c.execute("UPDATE project_invites SET state = 'revoked' WHERE id = ?", [invite])?;
            record(c, now, "member.invite_revoke", project, actor, None, json!({ "invite": invite }))?;
            revoked(c, now, project, invite, &role, envs_of(envs.clone()), "revoked", &actor.id)?;
            Ok(())
        })?;
        self.run_events();
        Ok(())
    }

    /// The pending invites for `user`'s address.
    pub fn my_invites(&self, user: &str) -> Result<Vec<Invite>> {
        let Some(email) = self.person(user).and_then(|p| p.email) else { return Ok(vec![]) };
        let now = self.db.now_ms()?;
        self.db.read(|c| invite_rows(c, "i.email = ? AND i.state = 'pending' AND i.expires_at > ?", params![email.to_ascii_lowercase(), now]))
    }

    /// Accept the invite `token` names, as `user`: only when it's for their address.
    pub fn accept(&self, user: &str, token: &str, ip: Option<String>) -> Result<Member> {
        let project = self.answer(user, token, ip, true)?;
        self.members(Scope::All, &project, false)?.into_iter().find(|m| m.user == user).ok_or_else(|| not_found(&project))
    }

    pub fn decline(&self, user: &str, token: &str, ip: Option<String>) -> Result<()> {
        self.answer(user, token, ip, false).map(|_| ())
    }

    fn answer(&self, user: &str, token: &str, ip: Option<String>, accept: bool) -> Result<String> {
        // The same answer for every way it can fail: an invite can't be used to learn who has an
        // account, or which addresses were invited.
        let gone = || Error::NotFound("this invite was used, revoked or has expired, or it's for another address".into());
        let person = self.person(user);
        let handle = person.as_ref().map_or_else(|| user.to_string(), |p| p.handle.clone());
        let email = person.and_then(|p| p.email).ok_or_else(gone)?.to_ascii_lowercase();
        let now = self.db.now_ms()?;
        // Accepting gives the role the invite names: only while whoever invited may still give it
        // (removed, demoted, disabled, an admin no longer one, a token revoked: none of it stands).
        let pending: Option<(String, String, Option<String>, String)> = self.db.read(|c| {
            c.query_row("SELECT id, role, environments, created_by FROM project_invites WHERE token_hash = ? AND state = 'pending'", [hash_token(token)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()
        })?;
        let checked = match &pending {
            Some((id, role, envs, by)) if accept => {
                let project: String = self.db.read(|c| c.query_row("SELECT project_id FROM project_invites WHERE id = ?", [id.as_str()], |r| r.get(0)))?;
                let inviter = self.inviter(by);
                if inviter.as_ref().and_then(|a| a.user.as_deref()) == Some(user) {
                    return Err(Error::Forbidden("an invite you made yourself can't be accepted: a project admin who isn't you invites you".into()));
                }
                let still = Role::parse(role).is_some_and(|r| inviter.is_some_and(|a| a.may_grant(&project, r, envs_of(envs.clone()).as_deref())));
                if !still {
                    return Err(Error::Forbidden("whoever invited you can no longer give this role: ask a project admin for a new invite".into()));
                }
                Some(id.clone())
            }
            _ => None,
        };
        let actor = Actor { id: user.into(), user: Some(user.into()), ip, ..Default::default() };
        let project = self.db.write(|c| {
            let found = c
                .query_row("SELECT id, project_id, role, environments, email, expires_at, created_by FROM project_invites WHERE token_hash = ? AND state = 'pending'", [hash_token(token)], |r| {
                    Ok(Pending { id: r.get(0)?, project: r.get(1)?, role: r.get(2)?, envs: r.get(3)?, email: r.get(4)?, expires_at: r.get(5)?, invited_by: r.get(6)? })
                })
                .optional()?;
            let Some(Pending { id, project, role, envs, email: invited, expires_at, invited_by }) = found else { return Err(gone()) };
            if invited != email || expires_at <= now {
                return Err(gone());
            }
            // The invite checked above, still the same one.
            if accept && checked.as_deref() != Some(id.as_str()) {
                return Err(gone());
            }
            if !accept {
                c.execute("UPDATE project_invites SET state = 'revoked', used_by = ?, used_at = ? WHERE id = ?", params![user, now, id])?;
                record(c, now, "member.decline", &project, &actor, Some(user), json!({ "invite": id }))?;
                let data = json!({ "invite": id, "role": role, "environments": envs_of(envs), "reason": "declined", "user": user, "handle": handle });
                events::emit(c, &project, now, Event::InviteRevoked, &id, user, data)?;
                return Ok(project);
            }
            if role_of(c, &project, user)?.is_some() {
                return Err(Error::Conflict("you're a member of this project already: an admin changes your role".into()));
            }
            let members: i64 = c.query_row("SELECT COUNT(*) FROM members WHERE project_id = ?", [project.as_str()], |r| r.get(0))?;
            // Their projects through groups count too.
            let projects: i64 = c.query_row(
                "SELECT COUNT(*) FROM (SELECT project_id FROM members WHERE user_id = ?1 UNION SELECT gr.project_id FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gm.user_id = ?1) x",
                [user],
                |r| r.get(0),
            )?;
            if members >= MEMBERS_MAX || projects >= PROJECTS_MAX {
                return Err(Error::Conflict(format!("a project has at most {MEMBERS_MAX} members, and a person is in at most {PROJECTS_MAX} projects")));
            }
            c.execute("INSERT INTO members (project_id, user_id, role, environments, added_by, added_at) VALUES (?, ?, ?, ?, ?, ?)", params![project, user, role, envs, invited_by, now])?;
            c.execute("UPDATE project_invites SET state = 'used', used_by = ?, used_at = ? WHERE id = ?", params![user, now, id])?;
            record(c, now, "member.accept", &project, &actor, Some(user), json!({ "invite": id, "role": role, "environments": envs_of(envs.clone()) }))?;
            let data = json!({ "invite": id, "user": user, "handle": handle, "role": role, "environments": envs_of(envs.clone()) });
            events::emit(c, &project, now, Event::InviteAccepted, &id, user, data)?;
            let data = json!({ "user": user, "handle": handle, "role": role, "environments": envs_of(envs.clone()), "via": "invite", "invite": id, "invited_by": invited_by });
            events::emit(c, &project, now, Event::MemberAdded, user, user, data)?;
            Ok(project)
        })?;
        self.run_events();
        Ok(project)
    }

    /// The project `kind ref` was made through, or set to (its home).
    pub fn home_of(&self, kind: Kind, r#ref: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT project_id FROM homes WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref], |r| r.get(0)).optional())?)
    }

    /// Make `project` the home of `kind ref`. `only_if_none`: a create's placement, which never
    /// moves a home that's set. Moving one away from a project takes being its admin too.
    pub fn set_home(&self, actor: &Actor, project: &str, kind: Kind, r#ref: &str, only_if_none: bool) -> Result<()> {
        crate::check_ref(r#ref)?;
        if !HOME_KINDS.contains(&kind) {
            // A create placed through a project: it just gets no home.
            if only_if_none {
                return Ok(());
            }
            return Err(Error::BadRequest(format!("a {} goes by a name that can be made again, so it has no home: a project trusts it only as one of its owners'", kind.as_str())));
        }
        // A person makes a project the home of only what's their own (`Owners`): their grant's rules
        // can name any resource, so they prove nothing about whose it is.
        let owns = actor.user.as_deref().is_none_or(|u| self.owned_by(u, kind, r#ref));
        let now = self.db.now_ms()?;
        let changed = self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let current: Option<String> = c.query_row("SELECT project_id FROM homes WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref], |r| r.get(0)).optional()?;
            match current {
                Some(p) if p == project => return Ok(false),
                Some(_) if only_if_none => return Ok(false),
                Some(from) => {
                    if let Some(user) = &actor.user
                        && !matches!(crate::groups::effective_role(c, &from, user)?, Some(Role::Admin | Role::Owner))
                    {
                        return Err(Error::Forbidden(format!("{} {} is at home in another project: only its admins move it", kind.as_str(), r#ref)));
                    }
                    c.execute("UPDATE homes SET project_id = ?, set_by = ?, set_at = ? WHERE kind = ? AND ref = ?", params![project, actor.id, now, kind.as_str(), r#ref])?;
                }
                None if !owns => {
                    return Err(Error::Forbidden(format!("{} {} isn't yours: only its owner, an admin or the operator makes a project its home", kind.as_str(), r#ref)));
                }
                None => {
                    c.execute("INSERT INTO homes (kind, ref, project_id, set_by, set_at) VALUES (?, ?, ?, ?, ?)", params![kind.as_str(), r#ref, project, actor.id, now])?;
                }
            }
            record(c, now, "home.set", project, actor, None, json!({ "kind": kind.as_str(), "ref": r#ref }))?;
            Ok(true)
        })?;
        if changed {
            for f in self.on_homed.read().expect("on_homed").iter() {
                f(kind, r#ref, project);
            }
            self.run_events();
        }
        Ok(())
    }

    /// Run `f(kind, ref, project)` after each resource gets a home, or a new one, outside the
    /// write: a product whose resources also have a person owner (search) hands it to the project
    /// there, so a home never stands for a stranger's resource.
    pub fn on_homed(&self, f: OnHomed) {
        self.on_homed.write().expect("on_homed").push(f);
    }

    /// Every home of `kind`: `(ref, project)`.
    pub fn homes(&self, kind: Kind) -> Result<Vec<(String, String)>> {
        Ok(self.db.read(|c| c.query("SELECT ref, project_id FROM homes WHERE kind = ? ORDER BY ref", [kind.as_str()], |r| Ok((r.get(0)?, r.get(1)?))))?)
    }

    /// The refs of `kind` at home in `project`.
    pub fn homes_in(&self, project: &str, kind: Kind) -> Result<Vec<String>> {
        Ok(self.db.read(|c| c.query("SELECT ref FROM homes WHERE project_id = ? AND kind = ? ORDER BY ref", [project, kind.as_str()], |r| r.get(0)))?)
    }

    /// The operator's: `kind ref` has no home any more (its places on canvases stay, trusted only
    /// as their projects' owners' own).
    pub fn drop_home(&self, kind: Kind, r#ref: &str) -> Result<()> {
        self.db.write(|c| c.execute("DELETE FROM homes WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref]).map(|_| ()))?;
        Ok(())
    }

    /// Act on the events not yet done: each claimed for a minute in a write first, so two nodes
    /// don't run one at once, and done once every hook took it. rolld also calls this now and
    /// then, for what a crash left.
    pub fn run_events(&self) {
        // One round after a request's write: a group's fan-out is worked off by rolld's tick
        // ([`Workspace::drain_events`]), never inside the request.
        self.run_events_once();
    }

    /// Rounds of [`Workspace::run_events`] until nothing is due, at most `rounds` (rolld's tick).
    pub fn drain_events(&self, rounds: usize) {
        for _ in 0..rounds {
            if self.run_events_once() == 0 {
                break;
            }
        }
    }

    /// One round of [`Workspace::run_events`]: how many events it claimed.
    fn run_events_once(&self) -> usize {
        if self.on_event.read().expect("on_event").is_empty() {
            return 0;
        }
        let Ok(now) = self.db.now_ms() else { return 0 };
        let claimed = self.db.write(|c| {
            let due: Vec<MemberEvent> = c.query(
                // Members' own events first: a big group's fan-out never holds up a removal (what
                // ends its person's shares and leases).
                "SELECT id, at, action, project_id, actor, subject, detail, ip FROM member_events WHERE done = 0 AND (claimed_at IS NULL OR claimed_at < ?) \
                 ORDER BY CASE WHEN action LIKE 'group.%' THEN 1 ELSE 0 END, at, id LIMIT 50",
                [now - 60_000],
                |r| {
                    let detail: String = r.get(6)?;
                    Ok(MemberEvent { id: r.get(0)?, at: r.get(1)?, action: r.get(2)?, project: r.get(3)?, actor: r.get(4)?, subject: r.get(5)?, detail: serde_json::from_str(&detail).unwrap_or(Value::Null), ip: r.get(7)? })
                },
            )?;
            for e in &due {
                c.execute("UPDATE member_events SET claimed_at = ? WHERE id = ?", params![now, e.id])?;
            }
            Ok::<_, Error>(due)
        });
        let Ok(claimed) = claimed else { return 0 };
        let n = claimed.len();
        let hooks = self.on_event.read().expect("on_event");
        for e in claimed {
            // A group's event first becomes one `group.lost` per person and project it cut off,
            // a page per round; its own hooks (the audit) run once that's done.
            if e.detail.get("fanout") == Some(&Value::Bool(true)) {
                match self.fan_out(&e) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(err) => {
                        tracing::warn!(event = %e.id, error = %err, "workspace: a group event's fan-out failed; it's tried again");
                        continue;
                    }
                }
            }
            let mut ok = true;
            for f in hooks.iter() {
                if let Err(err) = f(&e) {
                    tracing::warn!(event = %e.id, action = %e.action, error = %err, "workspace: a member event's hook failed; it's tried again");
                    ok = false;
                }
            }
            if ok {
                let _ = self.db.write(|c| c.execute("UPDATE member_events SET done = 1 WHERE id = ?", [e.id.as_str()]));
            }
        }
        n
    }

    /// Whether `project` trusts `kind ref` through its home or its owners' resources
    /// ([`Workspace::trusts`]).
    pub(crate) fn trusted_by_members(&self, project: &str, namespace: &str, kind: Kind, r#ref: &str) -> bool {
        self.trusted_with(project, namespace, &self.owners_of(project).unwrap_or_default(), kind, r#ref)
    }

    /// [`Workspace::trusted_by_members`] with the project's owners read already.
    fn trusted_with(&self, project: &str, namespace: &str, project_owners: &[String], kind: Kind, r#ref: &str) -> bool {
        if roles::decisions::HOMES && HOME_KINDS.contains(&kind) && self.home_of(kind, r#ref).ok().flatten().as_deref() == Some(project) {
            return true;
        }
        let Some(owners) = self.owners.get() else { return false };
        // The person who made the project as before members (an admin's project is the operator's,
        // D9); another owner adds only what's their own, never all of an admin account's reach.
        project_owners.iter().any(|o| if o == namespace { owners.trusts(o, kind, r#ref) } else { owners.owns(o, kind, r#ref) })
    }

    /// What [`Workspace::trusts`] reads of `project` itself (its namespace and owners), read once
    /// to ask about many resources ([`Workspace::trusts_on`]). `None`: no such project.
    pub fn trust_basis(&self, project: &str) -> Result<Option<TrustBasis>> {
        let namespace: Option<String> = self.db.read(|c| c.query_row("SELECT owner FROM projects WHERE id = ?", [project], |r| r.get(0)).optional())?;
        let Some(namespace) = namespace else { return Ok(None) };
        let owners = if namespace.is_empty() { Vec::new() } else { self.owners_of(project)? };
        Ok(Some(TrustBasis { namespace, owners }))
    }

    /// [`Workspace::trusts`] on a project's [`TrustBasis`].
    pub fn trusts_on(&self, project: &str, basis: &TrustBasis, kind: Kind, r#ref: &str) -> bool {
        basis.namespace.is_empty() || self.trusted_with(project, &basis.namespace, &basis.owners, kind, r#ref)
    }

    /// A product deleted `kind ref`: its home and its places on every canvas go, so nothing made
    /// later under the same ref inherits them.
    pub fn forget_resource(&self, kind: Kind, r#ref: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM homes WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref])?;
            c.execute("DELETE FROM resources WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref])?;
            Ok(())
        })
    }

    /// `user`'s account was deleted: they leave every project, through the same events as a
    /// removal (the audit log, D7's revocations, secrets' hook), and their pending invites go. A
    /// project left with no owner who can sign in is the operator's or an admin's to fix (no
    /// last-owner refusal: the account is gone either way).
    pub fn forget_person(&self, user: &str, ip: Option<String>) -> Result<usize> {
        let now = self.db.now_ms()?;
        let actor = Actor { id: "accounts".into(), ip, ..Default::default() };
        let n = self.db.write(|c| {
            let projects: Vec<(String, String, Option<String>)> = c.query("SELECT project_id, role, environments FROM members WHERE user_id = ?", [user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            for (project, role, envs) in &projects {
                c.execute("DELETE FROM members WHERE project_id = ? AND user_id = ?", [project.as_str(), user])?;
                record(c, now, "member.remove", project, &actor, Some(user), json!({ "role": role, "why": "account deleted" }))?;
                let data = json!({ "user": user, "handle": user, "role": role, "environments": envs_of(envs.clone()), "reason": "offboarded" });
                events::emit(c, project, now, Event::MemberRemoved, user, &actor.id, data)?;
            }
            // Their groups too, each a `group.member_remove`.
            let groups: Vec<String> = c.query("SELECT group_id FROM group_members WHERE user_id = ?", [user], |r| r.get(0))?;
            for g in &groups {
                crate::groups::remove_members(c, now, g, &actor, &[user.to_string()])?;
            }
            let invites: Vec<(String, String, String, Option<String>)> = c.query("SELECT id, project_id, role, environments FROM project_invites WHERE created_by = ? AND state = 'pending'", [user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            for (id, project, role, envs) in &invites {
                c.execute("UPDATE project_invites SET state = 'revoked' WHERE id = ?", [id.as_str()])?;
                record(c, now, "member.invite_revoke", project, &actor, Some(user), json!({ "invite": id, "why": "its inviter's account was deleted" }))?;
                revoked(c, now, project, id, role, envs_of(envs.clone()), "inviter_offboarded", &actor.id)?;
            }
            Ok::<_, Error>(projects.len())
        })?;
        self.run_events();
        Ok(n)
    }

    /// `user` lost rights in roll (roll accounts' offboarding outbox: disabled by an admin or SCIM,
    /// no longer an admin): when they can't sign in now, a `member.deactivated` in each project
    /// they reach, directly or through a group, in one write. Their memberships stay (they count
    /// for nothing until the account is enabled again). Run again after another hook failed, a
    /// project whose latest event about them already says so gets no second one. How many
    /// projects it wrote to.
    pub fn person_deactivated(&self, user: &str) -> Result<usize> {
        if self.person(user).is_some() {
            return Ok(0);
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let rows: Vec<(String, String, i64)> = c.query(
                "SELECT project_id, role, 0 FROM members WHERE user_id = ?1 UNION ALL \
                 SELECT gr.project_id, gr.role, 1 FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gm.user_id = ?1",
                [user],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let mut best: std::collections::BTreeMap<String, Role> = std::collections::BTreeMap::new();
            for (project, role, via) in rows {
                let role = if via == 1 { crate::groups::group_role(&role) } else { Role::parse(&role).unwrap_or(Role::Viewer) };
                let e = best.entry(project).or_insert(role);
                *e = (*e).max(role);
            }
            let mut n = 0;
            for (project, role) in best {
                if events::last_about(c, &project, user)?.as_deref() == Some(Event::MemberDeactivated.as_str()) {
                    continue;
                }
                events::emit(c, &project, now, Event::MemberDeactivated, user, "accounts", json!({ "user": user, "handle": user, "role": role }))?;
                n += 1;
            }
            Ok::<_, Error>(n)
        })
    }

    /// An invite's token was found leaked (roll-access's leak scanning, `rlv_`): it stops working.
    /// What was revoked, or `None` when it wasn't a live invite.
    pub fn revoke_leaked_invite(&self, token: &str) -> Option<String> {
        let now = self.db.now_ms().ok()?;
        let actor = Actor { id: "roll:leak".into(), ..Default::default() };
        self.db
            .write(|c| {
                let found: Option<(String, String, String, Option<String>)> = c.query_row("SELECT id, project_id, role, environments FROM project_invites WHERE token_hash = ? AND state = 'pending'", [hash_token(token)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
                let Some((id, project, role, envs)) = found else { return Ok(None) };
                c.execute("UPDATE project_invites SET state = 'revoked' WHERE id = ?", [id.as_str()])?;
                record(c, now, "member.invite_revoke", &project, &actor, None, json!({ "invite": id, "why": "leaked" }))?;
                revoked(c, now, &project, &id, &role, envs_of(envs), "leaked", &actor.id)?;
                Ok::<_, Error>(Some(format!("invite {id} to project {project}")))
            })
            .ok()
            .flatten()
    }
}

/// A pending invite as `answer` reads it.
struct Pending {
    id: String,
    project: String,
    role: String,
    envs: Option<String>,
    email: String,
    expires_at: i64,
    invited_by: String,
}

fn invite_rows(c: &Conn<'_>, filter: &str, p: impl code_sql::Params) -> Result<Vec<Invite>> {
    Ok(c.query(
        &format!("SELECT i.id, i.project_id, i.email, i.role, i.environments, i.state, i.created_by, i.created_at, i.expires_at, p.name FROM project_invites i JOIN projects p ON p.id = i.project_id WHERE {filter} ORDER BY i.created_at, i.id"),
        p,
        |r| {
            let role: String = r.get(3)?;
            Ok(Invite {
                id: r.get(0)?,
                project: r.get(1)?,
                email: r.get(2)?,
                role: Role::parse(&role).unwrap_or(Role::Viewer),
                environments: envs_of(r.get(4)?),
                state: r.get(5)?,
                created_by: r.get(6)?,
                created_at: r.get(7)?,
                expires_at: r.get(8)?,
                project_name: r.get(9)?,
            })
        },
    )?)
}

/// `user`'s pending invites to `project` that their role no longer gives (`now`: what it is now;
/// `None`: they left), revoked in the same write: an invite never outlives its inviter's rights.
fn revoke_invites_beyond(c: &Conn<'_>, now: i64, project: &str, user: &str, role: Option<(Role, Option<Vec<String>>)>, actor: &Actor) -> Result<()> {
    let pending: Vec<(String, String, Option<String>)> = c.query("SELECT id, role, environments FROM project_invites WHERE project_id = ? AND created_by = ? AND state = 'pending'", [project, user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let rules = role.map(|(role, environments)| roles::session_scope(&[roles::Membership { project: project.into(), role, environments }]));
    let manages = rules.as_ref().is_some_and(|s| {
        use roll_auth::scope::{Action, Outcome, Resource, Target};
        s.outcome(&Target::One(Resource { project: Some(project.into()), ..Resource::new("workspace", project) }), Action::Admin) == Outcome::Allow
    });
    for (id, r, e) in pending {
        let kept = manages && rules.as_ref().is_some_and(|s| Role::parse(&r).is_some_and(|r| roles::may_grant(s, project, r, envs_of(e.clone()).as_deref())));
        if !kept {
            c.execute("UPDATE project_invites SET state = 'revoked' WHERE id = ?", [id.as_str()])?;
            record(c, now, "member.invite_revoke", project, actor, Some(user), json!({ "invite": id, "why": "its inviter's role no longer gives it" }))?;
            revoked(c, now, project, &id, &r, envs_of(e), "inviter_role", &actor.id)?;
        }
    }
    Ok(())
}

/// `invite.revoked` in the feed, in the revoking write.
#[allow(clippy::too_many_arguments)]
fn revoked(c: &Conn<'_>, now: i64, project: &str, invite: &str, role: &str, environments: Option<Vec<String>>, reason: &str, actor: &str) -> Result<()> {
    let data = json!({ "invite": invite, "role": role, "environments": environments, "reason": reason });
    events::emit(c, project, now, Event::InviteRevoked, invite, actor, data).map(|_| ())
}

/// `workspace-members`: every project's maker becomes its owner, so everyone keeps what they had.
pub(crate) fn migrate(c: &Conn<'_>) -> code_sql::Result<()> {
    c.execute_batch(
        "INSERT INTO members (project_id, user_id, role, environments, added_by, added_at)
         SELECT id, owner, 'owner', NULL, owner, created_at FROM projects WHERE owner <> ''
         AND NOT EXISTS (SELECT 1 FROM members m WHERE m.project_id = projects.id AND m.user_id = projects.owner);",
    )
}

/// A new project's maker is its owner (the operator's projects have none).
pub(crate) fn first_owner(c: &Conn<'_>, project: &str, owner: &str, now: i64) -> Result<()> {
    if !owner.is_empty() {
        c.execute("INSERT INTO members (project_id, user_id, role, environments, added_by, added_at) VALUES (?, ?, 'owner', NULL, ?, ?)", params![project, owner, owner, now])?;
    }
    Ok(())
}

/// A deleted project's members and homes, kept apart in its write (they come back with it:
/// [`bring_back`]); its invites end, so no `rlv_` link of it is accepted again.
pub(crate) fn put_aside(c: &Conn<'_>, project: &str) -> Result<()> {
    c.execute("INSERT INTO deleted_members (project_id, user_id, role, environments, added_by, added_at) SELECT project_id, user_id, role, environments, added_by, added_at FROM members WHERE project_id = ?", [project])?;
    c.execute("INSERT INTO deleted_homes (kind, ref, project_id, set_by, set_at) SELECT kind, ref, project_id, set_by, set_at FROM homes WHERE project_id = ?", [project])?;
    for t in ["members", "project_invites", "homes"] {
        c.execute(&format!("DELETE FROM {t} WHERE project_id = ?"), [project])?;
    }
    crate::groups::put_aside(c, project)
}

/// A restored project's members and homes, back as they were (a home another project took since
/// stays there).
pub(crate) fn bring_back(c: &Conn<'_>, project: &str) -> Result<()> {
    c.execute(
        "INSERT INTO members (project_id, user_id, role, environments, added_by, added_at) SELECT project_id, user_id, role, environments, added_by, added_at FROM deleted_members WHERE project_id = ? ON CONFLICT (project_id, user_id) DO NOTHING",
        [project],
    )?;
    c.execute("INSERT INTO homes (kind, ref, project_id, set_by, set_at) SELECT kind, ref, project_id, set_by, set_at FROM deleted_homes WHERE project_id = ? ON CONFLICT (kind, ref) DO NOTHING", [project])?;
    Ok(())
}

/// A kept deleted project's members and homes go.
pub(crate) fn forget_aside(c: &Conn<'_>, project: &str) -> Result<()> {
    c.execute("DELETE FROM deleted_members WHERE project_id = ?", [project])?;
    c.execute("DELETE FROM deleted_homes WHERE project_id = ?", [project])?;
    crate::groups::forget_aside(c, project)
}
