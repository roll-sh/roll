//! Groups as role holders (docs/members-v1-1.md, "Groups"): a group is a set of people, and a
//! group's role on a project reaches every one of them. A person's rules on a project are the
//! union of their own membership's and each of their groups' roles there ([`Workspace::memberships_of`],
//! one indexed read per request, never cached).
//!
//! - A group never holds `owner` (D10): owners stay people, so `Workspace::trusts` and the last
//!   owner rule read people only.
//! - roll's own groups (`source: "roll"`) are the operator's and roll's admins' to make and fill
//!   (D12). A SCIM group (`scim:<connection>`) is its identity provider's: only SCIM changes its
//!   name and members (403 `managed` to the routes), while roll gives it its roles.
//! - The operator, or a project admin who is in the group, gives a group a role on a project
//!   (D11), and only a role their own rules cover (`Actor::may_grant`).
//! - Every change that takes access away records one event in its write with the people it may
//!   have cut off kept beside it (`group_fanout`); [`Workspace::run_events`] turns it into one
//!   `group.lost` event per person and project, a page at a time, so roll-access revokes their
//!   confined tokens and roll secrets ends what needs the role. Their next request is decided
//!   without the role anyway: grants are read live.

use crate::members::{Actor, MemberEvent, envs_json, envs_of, random, record};
use crate::{Error, Result, Scope, Workspace, owner_of};
use code_sql::{Conn, OptionalExtension, params};
use roll_auth::roles::{self, GroupAssigners, Role, decisions};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS groups (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL,
    external_id TEXT,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (source, name)
) WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS groups_external ON groups (source, external_id) WHERE external_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS group_members (
    group_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS group_members_user ON group_members (user_id);
CREATE TABLE IF NOT EXISTS group_roles (
    project_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    role TEXT NOT NULL,
    environments TEXT,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, project_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS group_roles_project ON group_roles (project_id);
CREATE TABLE IF NOT EXISTS deleted_group_roles (
    project_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    role TEXT NOT NULL,
    environments TEXT,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, group_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS group_fanout (
    event_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    PRIMARY KEY (event_id, user_id)
) WITHOUT ROWID;
";

/// The most groups on the server.
pub const GROUPS_MAX: i64 = 10_000;
/// The most people in one group.
pub const GROUP_MEMBERS_MAX: i64 = 10_000;
/// The most groups a person is in.
pub const GROUPS_PER_PERSON_MAX: i64 = 100;
/// The most projects one group has a role on.
pub const GROUP_PROJECTS_MAX: i64 = 200;
/// The most groups with a role on one project.
pub const PROJECT_GROUPS_MAX: i64 = 50;
/// The longest group name, in characters.
pub const GROUP_NAME_MAX: usize = 255;
/// People one page of a group's members holds, at most.
pub const MEMBERS_PAGE_MAX: i64 = 500;
/// People one fan-out step looks at (one `group.lost` each, at most).
const FANOUT_PAGE: usize = 500;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    /// `grp_` and 12 random characters of lowercase base32.
    pub id: String,
    pub name: String,
    pub description: String,
    /// `roll`, or `scim:<SSO connection id>`: only SCIM changes such a group's name and members.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// How many people are in it.
    pub members: i64,
    pub created_by: String,
    pub created_at: i64,
    /// The person asking is in it (only in a person's list): who may give it a role (D11).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub yours: bool,
}

impl Group {
    /// The SSO connection that keeps it, for a SCIM group.
    pub fn connection(&self) -> Option<&str> {
        self.source.strip_prefix("scim:")
    }
}

/// A group's role on one project.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupRole {
    pub project: String,
    pub group: String,
    pub name: String,
    pub source: String,
    pub role: Role,
    pub environments: Option<Vec<String>>,
    /// How many people the role reaches.
    pub members: i64,
    pub added_by: String,
    pub added_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupMember {
    pub user: String,
    pub handle: String,
    /// Only for the operator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub added_at: i64,
}

/// What a SCIM `PATCH` does, in order, in one write (the accounts side parsed and bounded it).
#[derive(Clone, Debug, PartialEq)]
pub enum GroupOp {
    AddMembers(Vec<String>),
    RemoveMembers(Vec<String>),
    /// The members are exactly these.
    ReplaceMembers(Vec<String>),
    Rename(String),
    ExternalId(Option<String>),
}

/// How many people a SCIM change added, removed, and skipped (people roll keeps out of groups:
/// one already in [`GROUPS_PER_PERSON_MAX`] groups).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub added: usize,
    pub removed: usize,
    pub skipped: usize,
}

const GROUP_COLUMNS: &str = "g.id, g.name, g.description, g.source, g.external_id, g.created_by, g.created_at, (SELECT COUNT(*) FROM group_members m WHERE m.group_id = g.id)";

fn group_row(r: &code_sql::Row<'_>) -> code_sql::Result<Group> {
    Ok(Group { id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, source: r.get(3)?, external_id: r.get(4)?, created_by: r.get(5)?, created_at: r.get(6)?, members: r.get(7)?, yours: false })
}

fn load(c: &Conn<'_>, id: &str) -> Result<Option<Group>> {
    Ok(c.query_row(&format!("SELECT {GROUP_COLUMNS} FROM groups g WHERE g.id = ?"), [id], group_row).optional()?)
}

fn no_group(id: &str) -> Error {
    Error::NotFound(format!("no group {id}"))
}

/// A group role as stored: an `owner` (a later version's, or a hand edit) reads as `admin` while
/// D10 holds, and a role the code doesn't know as `viewer`, as members are read.
pub(crate) fn group_role(s: &str) -> Role {
    match Role::parse(s) {
        Some(Role::Owner) if !decisions::GROUPS_HOLD_OWNER => Role::Admin,
        Some(r) => r,
        None => Role::Viewer,
    }
}

/// No control characters, trimmed, 1–255 characters.
fn check_name(name: &str) -> Result<String> {
    let n: String = name.trim().to_string();
    if n.is_empty() || n.chars().count() > GROUP_NAME_MAX || n.chars().any(char::is_control) {
        return Err(Error::BadRequest(format!("a group's name is 1–{GROUP_NAME_MAX} characters, no control characters")));
    }
    Ok(n)
}

fn check_description(d: &str) -> Result<()> {
    if d.chars().count() > crate::DESCRIPTION_MAX || d.chars().any(|c| c.is_control() && c != '\n') {
        return Err(Error::BadRequest(format!("a group's description is at most {} characters", crate::DESCRIPTION_MAX)));
    }
    Ok(())
}

fn limit(what: &str) -> Error {
    Error::Limit(what.to_string())
}

/// The projects `group` holds a role on.
fn projects_of(c: &Conn<'_>, group: &str) -> Result<Vec<String>> {
    Ok(c.query("SELECT project_id FROM group_roles WHERE group_id = ? ORDER BY project_id", [group], |r| r.get(0))?)
}

/// The roles `group` holds, as memberships (what its people lose when it goes).
fn held_by(c: &Conn<'_>, group: &str) -> Result<Vec<roles::Membership>> {
    Ok(c.query("SELECT project_id, role, environments FROM group_roles WHERE group_id = ? ORDER BY project_id", [group], |r| {
        let role: String = r.get(1)?;
        Ok(roles::Membership { project: r.get(0)?, role: group_role(&role), environments: envs_of(r.get(2)?) })
    })?)
}

/// `user`'s memberships on `project` now: their own and their groups'.
fn memberships_on(c: &Conn<'_>, project: &str, user: &str) -> Result<Vec<roles::Membership>> {
    let rows: Vec<(String, Option<String>, i64)> = c.query(
        "SELECT role, environments, 0 FROM members WHERE project_id = ?1 AND user_id = ?2 UNION ALL \
         SELECT gr.role, gr.environments, 1 FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gr.project_id = ?1 AND gm.user_id = ?2",
        [project, user],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(rows
        .into_iter()
        .map(|(role, envs, via)| roles::Membership { project: project.into(), role: if via == 1 { group_role(&role) } else { Role::parse(&role).unwrap_or(Role::Viewer) }, environments: envs_of(envs) })
        .collect())
}

/// Whether rules `now` still give everything `lost` did on its project (nothing was lost).
fn still_covered(now: &[roles::Membership], lost: &roles::Membership) -> bool {
    roles::may_grant(&roles::session_scope(now), &lost.project, lost.role, lost.environments.as_deref())
}

/// An event about a group that may have cut people off: `who` lost whatever `lost` gave them.
/// Nothing to fan out (no roles, or nobody) records the event alone.
fn record_loss(c: &Conn<'_>, now: i64, action: &str, project: &str, actor: &Actor, loss: Loss<'_>, mut detail: Value) -> Result<()> {
    let Loss { group, lost, who } = loss;
    let id = format!("mev_{}", random(16));
    let fanout = !lost.is_empty()
        && match who {
            Who::Everyone => {
                c.execute("INSERT INTO group_fanout (event_id, user_id) SELECT ?, user_id FROM group_members WHERE group_id = ?", [id.as_str(), group])? > 0
            }
            Who::These(list) => {
                for u in list {
                    c.execute("INSERT INTO group_fanout (event_id, user_id) VALUES (?, ?) ON CONFLICT (event_id, user_id) DO NOTHING", [id.as_str(), u.as_str()])?;
                }
                !list.is_empty()
            }
        };
    detail["group"] = json!(group);
    if fanout {
        detail["fanout"] = json!(true);
        detail["lost"] = lost.iter().map(|m| json!({ "project": m.project, "role": m.role, "environments": m.environments })).collect();
    }
    c.execute(
        "INSERT INTO member_events (id, at, action, project_id, actor, subject, detail, ip) VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
        params![id, now, action, project, actor.id, detail.to_string(), actor.ip],
    )?;
    Ok(())
}

/// What a group change may have taken: the roles `lost` that `group` gave `who`.
struct Loss<'a> {
    group: &'a str,
    lost: &'a [roles::Membership],
    who: Who<'a>,
}

enum Who<'a> {
    /// The group's members, as they are in the write (before they go, for a delete).
    Everyone,
    These(&'a [String]),
}

/// Whether a person (`?2`) holds any role on a project (`?1`): their membership, or one of their
/// groups' roles.
pub(crate) const REACHES: &str = "(EXISTS (SELECT 1 FROM members m WHERE m.project_id = ?1 AND m.user_id = ?2) OR EXISTS (SELECT 1 FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gr.project_id = ?1 AND gm.user_id = ?2))";

/// `user`'s highest role on `project`: their own membership's or a group's.
pub(crate) fn effective_role(c: &Conn<'_>, project: &str, user: &str) -> Result<Option<Role>> {
    let direct: Vec<String> = c.query("SELECT role FROM members WHERE project_id = ? AND user_id = ?", [project, user], |r| r.get(0))?;
    let through: Vec<String> = c.query("SELECT gr.role FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gr.project_id = ? AND gm.user_id = ?", [project, user], |r| r.get(0))?;
    Ok(direct.iter().map(|r| Role::parse(r).unwrap_or(Role::Viewer)).chain(through.iter().map(|r| group_role(r))).max())
}

/// The people in `group` that have now more than `max` distinct projects besides `project` (the
/// project a new group role would add): they'd pass [`crate::members::PROJECTS_MAX`].
fn over_projects(c: &Conn<'_>, group: &str, project: &str, max: i64) -> Result<i64> {
    Ok(c.query_row(
        "SELECT COUNT(*) FROM group_members gm WHERE gm.group_id = ?1 AND \
         (SELECT COUNT(*) FROM members m WHERE m.user_id = gm.user_id AND m.project_id <> ?2) + \
         (SELECT COUNT(DISTINCT gr.project_id) FROM group_members g2 JOIN group_roles gr ON gr.group_id = g2.group_id \
          WHERE g2.user_id = gm.user_id AND gr.project_id <> ?2 AND gr.project_id NOT IN (SELECT m2.project_id FROM members m2 WHERE m2.user_id = gm.user_id)) >= ?3 \
         AND NOT EXISTS (SELECT 1 FROM members m3 WHERE m3.user_id = gm.user_id AND m3.project_id = ?2)",
        params![group, project, max],
        |r| r.get(0),
    )?)
}

impl Workspace {
    /// Who may see `id`: the operator (`None`); else a person who is in it, or an admin of a
    /// project it has a role on.
    fn sees_group(c: &Conn<'_>, viewer: Option<&str>, id: &str) -> Result<bool> {
        let Some(user) = viewer else { return Ok(true) };
        let inside = c.query_row("SELECT 1 FROM group_members WHERE group_id = ? AND user_id = ?", [id, user], |r| r.get::<_, i64>(0)).optional()?.is_some();
        if inside {
            return Ok(true);
        }
        for p in projects_of(c, id)? {
            if matches!(effective_role(c, &p, user)?, Some(Role::Admin | Role::Owner)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The groups `viewer` sees (`None`: the operator, all of them), by name.
    pub fn groups(&self, viewer: Option<&str>) -> Result<Vec<Group>> {
        self.db.read(|c| {
            let all: Vec<Group> = match viewer {
                None => c.query(&format!("SELECT {GROUP_COLUMNS} FROM groups g ORDER BY g.name, g.id"), (), group_row)?,
                Some(user) => {
                    // Theirs, and those with a role on a project they manage.
                    let mine = format!(
                        "SELECT {GROUP_COLUMNS} FROM groups g WHERE g.id IN (SELECT group_id FROM group_members WHERE user_id = ?1) \
                         OR g.id IN (SELECT gr.group_id FROM group_roles gr WHERE gr.project_id IN \
                           (SELECT project_id FROM members WHERE user_id = ?1 AND role IN ('admin', 'owner') \
                            UNION SELECT r2.project_id FROM group_members m2 JOIN group_roles r2 ON r2.group_id = m2.group_id WHERE m2.user_id = ?1 AND r2.role IN ('admin', 'owner'))) \
                         ORDER BY g.name, g.id"
                    );
                    let inside: HashSet<String> = c.query("SELECT group_id FROM group_members WHERE user_id = ?", [user], |r| r.get(0))?.into_iter().collect();
                    c.query(&mine, [user], group_row)?.into_iter().map(|g| Group { yours: inside.contains(&g.id), ..g }).collect()
                }
            };
            Ok(all)
        })
    }

    pub fn group(&self, viewer: Option<&str>, id: &str) -> Result<Group> {
        self.db.read(|c| {
            if !Self::sees_group(c, viewer, id)? {
                return Err(no_group(id));
            }
            load(c, id)?.ok_or_else(|| no_group(id))
        })
    }

    /// A page of `id`'s members, by user id after `after`. Emails only for the operator.
    pub fn group_members(&self, viewer: Option<&str>, id: &str, after: &str, limit: i64) -> Result<Vec<GroupMember>> {
        let limit = limit.clamp(1, MEMBERS_PAGE_MAX);
        let rows: Vec<(String, i64)> = self.db.read(|c| {
            if !Self::sees_group(c, viewer, id)? || load(c, id)?.is_none() {
                return Err(no_group(id));
            }
            Ok(c.query("SELECT user_id, added_at FROM group_members WHERE group_id = ? AND user_id > ? ORDER BY user_id LIMIT ?", params![id, after, limit], |r| Ok((r.get(0)?, r.get(1)?)))?)
        })?;
        Ok(rows
            .into_iter()
            .map(|(user, added_at)| {
                let p = self.person(&user);
                GroupMember { handle: p.as_ref().map_or_else(|| user.clone(), |p| p.handle.clone()), email: p.and_then(|p| p.email).filter(|_| viewer.is_none()), user, added_at }
            })
            .collect())
    }

    /// A new group of `source` (`roll`, or `scim:<connection>`), with `members` in it.
    fn new_group(&self, actor: &Actor, source: &str, name: &str, description: &str, external_id: Option<&str>, members: &[String]) -> Result<(Group, Counts)> {
        let name = check_name(name)?;
        check_description(description)?;
        let id = format!("grp_{}", random(12));
        let now = self.db.now_ms()?;
        let counts = self.db.write(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM groups", (), |r| r.get(0))?;
            if n >= GROUPS_MAX {
                return Err(limit(&format!("the server has at most {GROUPS_MAX} groups")));
            }
            if c.query_row("SELECT 1 FROM groups WHERE source = ? AND name = ?", [source, name.as_str()], |r| r.get::<_, i64>(0)).optional()?.is_some() {
                return Err(Error::Conflict(format!("a group named {name} exists")));
            }
            if let Some(x) = external_id
                && c.query_row("SELECT 1 FROM groups WHERE source = ? AND external_id = ?", [source, x], |r| r.get::<_, i64>(0)).optional()?.is_some()
            {
                return Err(Error::Conflict(format!("a group with externalId {x} exists")));
            }
            c.execute("INSERT INTO groups (id, name, description, source, external_id, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)", params![id, name, description, source, external_id, actor.id, now])?;
            let counts = add_members(c, now, &id, actor, members)?;
            record(c, now, "group.create", "", actor, None, json!({ "group": id, "name": name, "source": source, "added": counts.added, "skipped": counts.skipped }))?;
            Ok::<_, Error>(counts)
        })?;
        self.run_events();
        Ok((self.db.read(|c| load(c, &id))?.ok_or_else(|| no_group(&id))?, counts))
    }

    /// A roll group (D12: the operator's and roll's admins', which `actor` is when it names no
    /// person).
    pub fn create_group(&self, actor: &Actor, name: &str, description: &str) -> Result<Group> {
        operator_only(actor, "making a group")?;
        self.new_group(actor, "roll", name, description, None, &[]).map(|(g, _)| g)
    }

    /// Rename a group or change its description: a SCIM group's name is its provider's.
    pub fn update_group(&self, actor: &Actor, id: &str, name: Option<&str>, description: Option<&str>) -> Result<Group> {
        operator_only(actor, "changing a group")?;
        let name = name.map(check_name).transpose()?;
        if let Some(d) = description {
            check_description(d)?;
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let g = load(c, id)?.ok_or_else(|| no_group(id))?;
            if let Some(n) = &name {
                if g.connection().is_some() && *n != g.name {
                    return Err(Error::Managed(format!("{} is kept by its identity provider: rename it there", g.name)));
                }
                if c.query_row("SELECT 1 FROM groups WHERE source = ? AND name = ? AND id <> ?", [g.source.as_str(), n.as_str(), id], |r| r.get::<_, i64>(0)).optional()?.is_some() {
                    return Err(Error::Conflict(format!("a group named {n} exists")));
                }
                c.execute("UPDATE groups SET name = ? WHERE id = ?", [n.as_str(), id])?;
            }
            if let Some(d) = description {
                c.execute("UPDATE groups SET description = ? WHERE id = ?", [d, id])?;
            }
            record(c, now, "group.update", "", actor, None, json!({ "group": id, "name": name, "description": description.is_some() }))?;
            Ok(())
        })?;
        self.run_events();
        self.group(None, id)
    }

    /// Delete a group: its members and its project roles go, and the people they reached lose
    /// it on their next request. A SCIM group is its provider's to delete, unless its
    /// connection's SCIM groups are off now.
    pub fn delete_group(&self, actor: &Actor, id: &str) -> Result<()> {
        operator_only(actor, "deleting a group")?;
        let g = self.group(None, id)?;
        if self.keeper(&g) == Keeper::Provider {
            return Err(Error::Managed(format!("{} is kept by its identity provider: delete it there, or turn the connection's SCIM groups off first", g.name)));
        }
        self.drop_group(actor, id)
    }

    fn drop_group(&self, actor: &Actor, id: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let g = load(c, id)?.ok_or_else(|| no_group(id))?;
            let lost = held_by(c, id)?;
            record_loss(c, now, "group.delete", "", actor, Loss { group: id, lost: &lost, who: Who::Everyone }, json!({ "name": g.name, "source": g.source }))?;
            for t in ["group_roles", "group_members", "groups"] {
                let col = if t == "groups" { "id" } else { "group_id" };
                c.execute(&format!("DELETE FROM {t} WHERE {col} = ?"), [id])?;
            }
            Ok::<_, Error>(())
        })?;
        self.run_events();
        Ok(())
    }

    /// Put `user` in roll group `id` (the operator's). 404 for a person who doesn't exist.
    pub fn add_group_member(&self, actor: &Actor, id: &str, user: &str) -> Result<()> {
        operator_only(actor, "changing a group's members")?;
        if self.person(user).is_none() {
            return Err(Error::NotFound(format!("no person {user}")));
        }
        let g = self.group(None, id)?;
        managed(&g, self.keeper(&g), false)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            load(c, id)?.ok_or_else(|| no_group(id))?;
            if c.query_row("SELECT 1 FROM group_members WHERE group_id = ? AND user_id = ?", [id, user], |r| r.get::<_, i64>(0)).optional()?.is_some() {
                return Ok(());
            }
            check_person_fits(c, id, user)?;
            if add_members(c, now, id, actor, &[user.to_string()])?.skipped > 0 {
                return Err(limit(&format!("a person is in at most {GROUPS_PER_PERSON_MAX} groups")));
            }
            record(c, now, "group.member_add", "", actor, Some(user), json!({ "group": id }))?;
            Ok(())
        })?;
        self.run_events();
        Ok(())
    }

    /// Take `user` out of group `id` (a SCIM group's only while its connection's SCIM groups are
    /// off): they lose its roles at once.
    pub fn remove_group_member(&self, actor: &Actor, id: &str, user: &str) -> Result<()> {
        operator_only(actor, "changing a group's members")?;
        let g = self.group(None, id)?;
        managed(&g, self.keeper(&g), true)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            load(c, id)?.ok_or_else(|| no_group(id))?;
            remove_members(c, now, id, actor, &[user.to_string()])?;
            Ok::<_, Error>(())
        })?;
        self.run_events();
        Ok(())
    }

    /// The groups with a role on `project` (`scope` must see it).
    pub fn project_groups(&self, scope: Scope<'_>, project: &str) -> Result<Vec<GroupRole>> {
        self.db.read(|c| {
            owner_of(c, scope, project)?;
            project_group_rows(c, "gr.project_id = ?", [project])
        })
    }

    /// Give `group` `role` on `project` (or change the role it has): never `owner` (D10); the
    /// role, and the one it replaces, covered by the actor's rules; and the actor the operator or
    /// in the group (D11).
    pub fn set_group_role(&self, actor: &Actor, project: &str, group: &str, role: &str, environments: Option<Vec<String>>) -> Result<GroupRole> {
        let role = Role::parse(role).ok_or_else(|| Error::BadRequest(format!("unknown role {role:?}: one of viewer, developer, admin")))?;
        if role == Role::Owner && !decisions::GROUPS_HOLD_OWNER {
            return Err(Error::BadRequest("a group never holds owner: owners are people (make someone an owner in Members)".into()));
        }
        crate::members::check_envs(role, &environments)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let g = load(c, group)?.ok_or_else(|| no_group(group))?;
            may_assign(c, actor, group)?;
            let old: Option<(String, Option<String>)> = c.query_row("SELECT role, environments FROM group_roles WHERE project_id = ? AND group_id = ?", [project, group], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let old = old.map(|(r, e)| (group_role(&r), envs_of(e)));
            if let Some((r, e)) = &old
                && !actor.may_grant(project, *r, e.as_deref())
            {
                return Err(Error::Forbidden(format!("you can't change {}'s role here: its {} is more than your own role gives", g.name, r.as_str())));
            }
            if !actor.may_grant(project, role, environments.as_deref()) {
                return Err(Error::Forbidden(format!("you can't give {} the role {}: it's more than your own role gives", g.name, role.as_str())));
            }
            if old.is_none() {
                let here: i64 = c.query_row("SELECT COUNT(*) FROM group_roles WHERE project_id = ?", [project], |r| r.get(0))?;
                if here >= PROJECT_GROUPS_MAX {
                    return Err(limit(&format!("a project gives roles to at most {PROJECT_GROUPS_MAX} groups")));
                }
                let theirs: i64 = c.query_row("SELECT COUNT(*) FROM group_roles WHERE group_id = ?", [group], |r| r.get(0))?;
                if theirs >= GROUP_PROJECTS_MAX {
                    return Err(limit(&format!("a group has a role on at most {GROUP_PROJECTS_MAX} projects")));
                }
                if over_projects(c, group, project, crate::members::PROJECTS_MAX)? > 0 {
                    return Err(limit(&format!("someone in {} would be in more than {} projects", g.name, crate::members::PROJECTS_MAX)));
                }
                c.execute("INSERT INTO group_roles (project_id, group_id, role, environments, added_by, added_at) VALUES (?, ?, ?, ?, ?, ?)", params![project, group, role.as_str(), envs_json(&environments), actor.id, now])?;
                record(c, now, "group.role", project, actor, None, json!({ "group": group, "new": { "role": role, "environments": environments } }))?;
            } else {
                c.execute("UPDATE group_roles SET role = ?, environments = ? WHERE project_id = ? AND group_id = ?", params![role.as_str(), envs_json(&environments), project, group])?;
                // Only a change that narrows the role can have cut someone off: theirs is checked again.
                let (r, e) = old.clone().expect("old");
                let was = roles::Membership { project: project.into(), role: r, environments: e.clone() };
                let is = roles::Membership { project: project.into(), role, environments: environments.clone() };
                let detail = json!({ "old": { "role": r, "environments": e }, "new": { "role": role, "environments": environments } });
                if still_covered(std::slice::from_ref(&is), &was) {
                    record(c, now, "group.role", project, actor, None, json!({ "group": group, "old": detail["old"].clone(), "new": detail["new"].clone() }))?;
                } else {
                    record_loss(c, now, "group.role", project, actor, Loss { group, lost: std::slice::from_ref(&was), who: Who::Everyone }, detail)?;
                }
            }
            Ok(())
        })?;
        self.run_events();
        self.db.read(|c| project_group_rows(c, "gr.project_id = ? AND gr.group_id = ?", [project, group]))?.pop().ok_or_else(|| no_group(group))
    }

    /// Take `group`'s role on `project` away (the actor's rules must cover it).
    pub fn remove_group_role(&self, actor: &Actor, project: &str, group: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            owner_of(c, actor.scope(), project)?;
            let found: Option<(String, Option<String>)> = c.query_row("SELECT role, environments FROM group_roles WHERE project_id = ? AND group_id = ?", [project, group], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((role, envs)) = found else { return Err(Error::NotFound(format!("{group} has no role in {project}"))) };
            let (role, envs) = (group_role(&role), envs_of(envs));
            if !actor.may_grant(project, role, envs.as_deref()) {
                return Err(Error::Forbidden(format!("you can't take away a {}: it's more than your own role gives", role.as_str())));
            }
            let was = roles::Membership { project: project.into(), role, environments: envs.clone() };
            record_loss(c, now, "group.role_remove", project, actor, Loss { group, lost: std::slice::from_ref(&was), who: Who::Everyone }, json!({ "role": role, "environments": envs }))?;
            c.execute("DELETE FROM group_roles WHERE project_id = ? AND group_id = ?", [project, group])?;
            Ok(())
        })?;
        self.run_events();
        Ok(())
    }

    /// Whether `user` holds any role on `project`, directly or through a group, now.
    pub fn reaches(&self, project: &str, user: &str) -> Result<bool> {
        self.db.read(|c| Ok(c.query_row(&format!("SELECT 1 WHERE {REACHES}"), [project, user], |r| r.get::<_, i64>(0)).optional()?.is_some()))
    }

    /// Every person with a role on `project` who can sign in, with their highest role (their own
    /// or a group's): who a `role:admin` approver is.
    pub fn roles_in(&self, project: &str) -> Result<Vec<(String, Role)>> {
        let rows: Vec<(String, String, bool)> = self.db.read(|c| {
            Ok::<_, Error>(c.query(
                "SELECT user_id, role, 0 FROM members WHERE project_id = ?1 UNION ALL \
                 SELECT gm.user_id, gr.role, 1 FROM group_roles gr JOIN group_members gm ON gm.group_id = gr.group_id WHERE gr.project_id = ?1",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? == 1)),
            )?)
        })?;
        let mut best: BTreeMap<String, Role> = BTreeMap::new();
        for (user, role, via_group) in rows {
            let role = if via_group { group_role(&role) } else { Role::parse(&role).unwrap_or(Role::Viewer) };
            let e = best.entry(user).or_insert(role);
            *e = (*e).max(role);
        }
        Ok(best.into_iter().filter(|(u, _)| self.person(u).is_some()).collect())
    }

    // ── SCIM (roll-accounts' `/Groups`, through rolld's `Directory`): only a connection's own
    // groups, and only people the accounts side said the connection reaches. ──

    fn scim_load(c: &Conn<'_>, conn: &str, id: &str) -> Result<Group> {
        load(c, id)?.filter(|g| g.connection() == Some(conn)).ok_or_else(|| no_group(id))
    }

    /// A connection's groups, filtered and paged in SQL: `by` is `("displayName" | "externalId" |
    /// "id", value)`.
    pub fn scim_groups(&self, conn: &str, by: Option<(&str, &str)>, offset: i64, count: i64) -> Result<(i64, Vec<Group>)> {
        let source = format!("scim:{conn}");
        let (clause, value) = match by {
            Some(("displayName", v)) => ("AND g.name = ?", Some(v)),
            Some(("externalId", v)) => ("AND g.external_id = ?", Some(v)),
            Some((_, v)) => ("AND g.id = ?", Some(v)),
            None => ("", None),
        };
        let args: Vec<&str> = std::iter::once(source.as_str()).chain(value).collect();
        self.db.read(|c| {
            let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM groups g WHERE g.source = ? {clause}"), &args, |r| r.get(0))?;
            let page = c.query(&format!("SELECT {GROUP_COLUMNS} FROM groups g WHERE g.source = ? {clause} ORDER BY g.created_at, g.id LIMIT {} OFFSET {}", count.max(0), offset.max(0)), &args, group_row)?;
            Ok((total, page))
        })
    }

    /// One of a connection's groups, with its members' ids (all of them, at most
    /// [`GROUP_MEMBERS_MAX`]).
    pub fn scim_group(&self, conn: &str, id: &str, members: bool) -> Result<(Group, Vec<GroupMember>)> {
        let (g, users): (Group, Vec<(String, i64)>) = self.db.read(|c| {
            let g = Self::scim_load(c, conn, id)?;
            let users = if members { c.query("SELECT user_id, added_at FROM group_members WHERE group_id = ? ORDER BY user_id", [id], |r| Ok((r.get(0)?, r.get(1)?)))? } else { vec![] };
            Ok::<_, Error>((g, users))
        })?;
        let people = users.into_iter().map(|(user, added_at)| GroupMember { handle: self.person(&user).map_or_else(|| user.clone(), |p| p.handle), email: None, user, added_at }).collect();
        Ok((g, people))
    }

    pub fn scim_create(&self, conn: &str, name: &str, external_id: Option<&str>, members: &[String]) -> Result<(Group, Counts)> {
        let actor = scim_actor(conn);
        self.new_group(&actor, &format!("scim:{conn}"), name, "", external_id, members)
    }

    /// Apply `ops` in order, in one write.
    pub fn scim_patch(&self, conn: &str, id: &str, ops: &[GroupOp]) -> Result<Counts> {
        let actor = scim_actor(conn);
        let now = self.db.now_ms()?;
        let counts = self.db.write(|c| {
            let g = Self::scim_load(c, conn, id)?;
            let mut total = Counts::default();
            let mut removed: Vec<String> = vec![];
            for op in ops {
                match op {
                    GroupOp::AddMembers(list) => {
                        let n = add_members(c, now, id, &actor, list)?;
                        total.added += n.added;
                        total.skipped += n.skipped;
                    }
                    GroupOp::RemoveMembers(list) => removed.extend(take_out(c, id, list)?),
                    GroupOp::ReplaceMembers(list) => {
                        let keep: HashSet<&String> = list.iter().collect();
                        let now_in: Vec<String> = c.query("SELECT user_id FROM group_members WHERE group_id = ?", [id], |r| r.get(0))?;
                        let gone: Vec<String> = now_in.into_iter().filter(|u| !keep.contains(u)).collect();
                        removed.extend(take_out(c, id, &gone)?);
                        let n = add_members(c, now, id, &actor, list)?;
                        total.added += n.added;
                        total.skipped += n.skipped;
                    }
                    GroupOp::Rename(name) => {
                        let name = check_name(name)?;
                        if c.query_row("SELECT 1 FROM groups WHERE source = ? AND name = ? AND id <> ?", [g.source.as_str(), name.as_str(), id], |r| r.get::<_, i64>(0)).optional()?.is_some() {
                            return Err(Error::Conflict(format!("a group named {name} exists")));
                        }
                        c.execute("UPDATE groups SET name = ? WHERE id = ?", [name.as_str(), id])?;
                    }
                    GroupOp::ExternalId(x) => {
                        let x = x.as_deref().map(str::trim).filter(|x| !x.is_empty());
                        if let Some(x) = x
                            && c.query_row("SELECT 1 FROM groups WHERE source = ? AND external_id = ? AND id <> ?", [g.source.as_str(), x, id], |r| r.get::<_, i64>(0)).optional()?.is_some()
                        {
                            return Err(Error::Conflict(format!("a group with externalId {x} exists")));
                        }
                        c.execute("UPDATE groups SET external_id = ? WHERE id = ?", params![x, id])?;
                    }
                }
            }
            // Someone removed and added back in the same request kept the group throughout.
            removed.sort();
            removed.dedup();
            let still: HashSet<String> = c.query("SELECT user_id FROM group_members WHERE group_id = ?", [id], |r| r.get(0))?.into_iter().collect();
            removed.retain(|u| !still.contains(u));
            total.removed = removed.len();
            let count: i64 = c.query_row("SELECT COUNT(*) FROM group_members WHERE group_id = ?", [id], |r| r.get(0))?;
            if count > GROUP_MEMBERS_MAX {
                return Err(Error::TooMany(format!("a group holds at most {GROUP_MEMBERS_MAX} people")));
            }
            let lost = held_by(c, id)?;
            record_loss(c, now, "group.update", "", &actor, Loss { group: id, lost: &lost, who: Who::These(&removed) }, json!({ "added": total.added, "removed": total.removed, "skipped": total.skipped }))?;
            Ok::<_, Error>(total)
        })?;
        self.run_events();
        Ok(counts)
    }

    pub fn scim_delete(&self, conn: &str, id: &str) -> Result<()> {
        self.db.read(|c| Self::scim_load(c, conn, id))?;
        self.drop_group(&scim_actor(conn), id)
    }

    /// An SSO connection was removed: its groups go, as if its provider deleted each.
    pub fn forget_connection_groups(&self, conn: &str) -> Result<usize> {
        let ids: Vec<String> = self.db.read(|c| Ok::<_, Error>(c.query("SELECT id FROM groups WHERE source = ?", [format!("scim:{conn}")], |r| r.get(0))?))?;
        for id in &ids {
            self.drop_group(&scim_actor(conn), id)?;
        }
        Ok(ids.len())
    }

    /// One step of a group event's fan-out: the next page of the people it kept, one `group.lost`
    /// event each, naming the projects where what they hold now no longer covers what the group
    /// gave (someone still there with as much, directly or through another group, lost nothing).
    /// `true` once every one is written (the event's own hooks run then).
    pub(crate) fn fan_out(&self, e: &MemberEvent) -> Result<bool> {
        let lost: Vec<roles::Membership> = e
            .detail
            .get("lost")
            .and_then(Value::as_array)
            .map(|l| {
                l.iter()
                    .filter_map(|m| Some(roles::Membership { project: m.get("project")?.as_str()?.to_string(), role: group_role(m.get("role")?.as_str()?), environments: m.get("environments").and_then(|e| serde_json::from_value(e.clone()).ok()) }))
                    .collect()
            })
            .unwrap_or_default();
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let detail: String = c.query_row("SELECT detail FROM member_events WHERE id = ?", [e.id.as_str()], |r| r.get(0))?;
            let mut detail: Value = serde_json::from_str(&detail).unwrap_or(Value::Null);
            if detail.get("fanout") != Some(&json!(true)) {
                return Ok(true);
            }
            let cursor = detail.get("cursor").and_then(Value::as_str).unwrap_or("").to_string();
            let users: Vec<String> = c.query("SELECT user_id FROM group_fanout WHERE event_id = ? AND user_id > ? ORDER BY user_id LIMIT ?", params![e.id, cursor, FANOUT_PAGE as i64], |r| r.get(0))?;
            let group = detail.get("group").cloned().unwrap_or(Value::Null);
            for u in &users {
                let mut projects = vec![];
                for l in &lost {
                    if !still_covered(&memberships_on(c, &l.project, u)?, l) {
                        projects.push(l.project.clone());
                    }
                }
                if projects.is_empty() {
                    continue;
                }
                c.execute(
                    "INSERT INTO member_events (id, at, action, project_id, actor, subject, detail, ip) VALUES (?, ?, 'group.lost', '', ?, ?, ?, ?)",
                    params![format!("mev_{}", random(16)), now, e.actor, u, json!({ "group": group, "event": e.id, "why": e.action, "projects": projects }).to_string(), e.ip],
                )?;
            }
            let done = users.len() < FANOUT_PAGE;
            if done {
                c.execute("DELETE FROM group_fanout WHERE event_id = ?", [e.id.as_str()])?;
                detail["fanout"] = json!(false);
            } else if let Some(last) = users.last() {
                detail["cursor"] = json!(last);
            }
            c.execute("UPDATE member_events SET detail = ?, claimed_at = NULL WHERE id = ?", [detail.to_string(), e.id.clone()])?;
            Ok::<_, Error>(done)
        })
    }
}

fn scim_actor(conn: &str) -> Actor {
    Actor { id: format!("scim:{conn}"), ..Default::default() }
}

/// The operator's (and roll's admins', D12): a person acting as themselves may not.
fn operator_only(actor: &Actor, what: &str) -> Result<()> {
    // The operator itself or an admin's session: never a scoped grant, which a token of either
    // carries.
    if (actor.user.is_some() || actor.grant.is_some()) && !decisions::PEOPLE_MAKE_GROUPS {
        return Err(Error::Forbidden(format!("{what} is roll's operator's or an admin's")));
    }
    Ok(())
}

/// What roll's routes may do to a SCIM group's members: nothing while its provider keeps them;
/// take people out while the connection's SCIM groups are off; anything once the connection is
/// gone (it's roll's then).
#[derive(Clone, Copy, PartialEq)]
enum Keeper {
    Provider,
    Paused,
    Roll,
}

impl Workspace {
    fn keeper(&self, g: &Group) -> Keeper {
        let Some(conn) = g.connection() else { return Keeper::Roll };
        let people = self.people.get();
        if people.is_some_and(|p| p.connection_gone(conn)) {
            Keeper::Roll
        } else if people.is_some_and(|p| p.scim_groups_on(conn)) {
            Keeper::Provider
        } else {
            Keeper::Paused
        }
    }

    /// Drop the groups of SSO connections that are gone (a removal that crashed before its groups
    /// went): rolld runs this at start and on its tick.
    pub fn sweep_groups(&self) -> Result<usize> {
        let Some(people) = self.people.get() else { return Ok(0) };
        let sources: Vec<String> = self.db.read(|c| Ok::<_, Error>(c.query("SELECT DISTINCT source FROM groups WHERE source LIKE 'scim:%'", (), |r| r.get(0))?))?;
        let mut n = 0;
        for s in sources {
            let conn = s.trim_start_matches("scim:");
            if people.connection_gone(conn) {
                n += self.forget_connection_groups(conn)?;
            }
        }
        Ok(n)
    }
}

fn managed(g: &Group, keeper: Keeper, removing: bool) -> Result<()> {
    match keeper {
        Keeper::Roll => Ok(()),
        Keeper::Paused if removing => Ok(()),
        _ => Err(Error::Managed(format!("{}'s members are kept by its identity provider", g.name))),
    }
}

/// D11: who may give `group` a role.
fn may_assign(c: &Conn<'_>, actor: &Actor, group: &str) -> Result<()> {
    // D11's operator is the operator itself or an admin's session, never a scoped token (an
    // operator's `rlt_` with admin on one project could otherwise hand any tenant's group a role).
    let Some(user) = &actor.user else {
        return if actor.grant.is_none() { Ok(()) } else { Err(Error::Forbidden("a token never gives a group a role: roll's operator or a project admin in the group does, signed in".into())) };
    };
    let ok = match decisions::GROUP_ASSIGNERS {
        GroupAssigners::AnyAdmin => true,
        GroupAssigners::Operator => false,
        GroupAssigners::OperatorOrMemberAdmin => c.query_row("SELECT 1 FROM group_members WHERE group_id = ? AND user_id = ?", [group, user.as_str()], |r| r.get::<_, i64>(0)).optional()?.is_some(),
    };
    if ok { Ok(()) } else { Err(Error::Forbidden("only roll's operator, or a project admin who is in the group, gives a group a role".into())) }
}

/// A person added to `group` mustn't pass [`crate::members::PROJECTS_MAX`] projects through it.
fn check_person_fits(c: &Conn<'_>, group: &str, user: &str) -> Result<()> {
    let n: i64 = c.query_row(
        "SELECT COUNT(*) FROM (SELECT project_id FROM members WHERE user_id = ?1 UNION \
         SELECT gr.project_id FROM group_members gm JOIN group_roles gr ON gr.group_id = gm.group_id WHERE gm.user_id = ?1 UNION \
         SELECT project_id FROM group_roles WHERE group_id = ?2) x",
        [user, group],
        |r| r.get(0),
    )?;
    if n > crate::members::PROJECTS_MAX {
        return Err(limit(&format!("they'd be in more than {} projects", crate::members::PROJECTS_MAX)));
    }
    let size: i64 = c.query_row("SELECT COUNT(*) FROM group_members WHERE group_id = ?", [group], |r| r.get(0))?;
    if size >= GROUP_MEMBERS_MAX {
        return Err(limit(&format!("a group holds at most {GROUP_MEMBERS_MAX} people")));
    }
    Ok(())
}

/// Add `users` to `group` (those in it already stay as they are); one in
/// [`GROUPS_PER_PERSON_MAX`] groups already is skipped.
fn add_members(c: &Conn<'_>, now: i64, group: &str, actor: &Actor, users: &[String]) -> Result<Counts> {
    let mut counts = Counts::default();
    for u in users {
        if c.query_row("SELECT 1 FROM group_members WHERE group_id = ? AND user_id = ?", [group, u.as_str()], |r| r.get::<_, i64>(0)).optional()?.is_some() {
            continue;
        }
        let theirs: i64 = c.query_row("SELECT COUNT(*) FROM group_members WHERE user_id = ?", [u.as_str()], |r| r.get(0))?;
        if theirs >= GROUPS_PER_PERSON_MAX {
            counts.skipped += 1;
            continue;
        }
        c.execute("INSERT INTO group_members (group_id, user_id, added_by, added_at) VALUES (?, ?, ?, ?)", params![group, u, actor.id, now])?;
        counts.added += 1;
    }
    let size: i64 = c.query_row("SELECT COUNT(*) FROM group_members WHERE group_id = ?", [group], |r| r.get(0))?;
    if size > GROUP_MEMBERS_MAX {
        return Err(Error::TooMany(format!("a group holds at most {GROUP_MEMBERS_MAX} people")));
    }
    Ok(counts)
}

/// Delete `users` from `group`: the ones who were in it.
fn take_out(c: &Conn<'_>, group: &str, users: &[String]) -> Result<Vec<String>> {
    let mut out = vec![];
    for u in users {
        if c.execute("DELETE FROM group_members WHERE group_id = ? AND user_id = ?", [group, u.as_str()])? > 0 {
            out.push(u.clone());
        }
    }
    Ok(out)
}

/// Take `users` out of `group`, recording what they lost.
pub(crate) fn remove_members(c: &Conn<'_>, now: i64, group: &str, actor: &Actor, users: &[String]) -> Result<()> {
    let removed = take_out(c, group, users)?;
    if removed.is_empty() {
        return Ok(());
    }
    let lost = held_by(c, group)?;
    let subject = (removed.len() == 1).then(|| removed[0].clone());
    record_loss(c, now, "group.member_remove", "", actor, Loss { group, lost: &lost, who: Who::These(&removed) }, json!({ "user": subject, "removed": removed.len() }))
}

fn project_group_rows(c: &Conn<'_>, filter: &str, p: impl code_sql::Params) -> Result<Vec<GroupRole>> {
    Ok(c.query(
        &format!(
            "SELECT gr.project_id, gr.group_id, g.name, g.source, gr.role, gr.environments, (SELECT COUNT(*) FROM group_members m WHERE m.group_id = gr.group_id), gr.added_by, gr.added_at \
             FROM group_roles gr JOIN groups g ON g.id = gr.group_id WHERE {filter} ORDER BY g.name, gr.group_id"
        ),
        p,
        |r| {
            let role: String = r.get(4)?;
            Ok(GroupRole { project: r.get(0)?, group: r.get(1)?, name: r.get(2)?, source: r.get(3)?, role: group_role(&role), environments: envs_of(r.get(5)?), members: r.get(6)?, added_by: r.get(7)?, added_at: r.get(8)? })
        },
    )?)
}

/// A deleted project's group roles, kept apart in its write ([`bring_back`] restores them).
pub(crate) fn put_aside(c: &Conn<'_>, project: &str) -> Result<()> {
    c.execute("INSERT INTO deleted_group_roles (project_id, group_id, role, environments, added_by, added_at) SELECT project_id, group_id, role, environments, added_by, added_at FROM group_roles WHERE project_id = ?", [project])?;
    c.execute("DELETE FROM group_roles WHERE project_id = ?", [project])?;
    Ok(())
}

/// A restored project's group roles, back as they were, except those of groups deleted since:
/// dropped, each in the audit log.
pub(crate) fn bring_back(c: &Conn<'_>, project: &str, now: i64) -> Result<Vec<String>> {
    let dropped: Vec<String> = c.query("SELECT d.group_id FROM deleted_group_roles d WHERE d.project_id = ? AND NOT EXISTS (SELECT 1 FROM groups g WHERE g.id = d.group_id) ORDER BY d.group_id", [project], |r| r.get(0))?;
    c.execute(
        "INSERT INTO group_roles (project_id, group_id, role, environments, added_by, added_at) SELECT d.project_id, d.group_id, d.role, d.environments, d.added_by, d.added_at FROM deleted_group_roles d \
         WHERE d.project_id = ? AND EXISTS (SELECT 1 FROM groups g WHERE g.id = d.group_id) ON CONFLICT (group_id, project_id) DO NOTHING",
        [project],
    )?;
    let roll = Actor { id: "roll:restore".into(), ..Default::default() };
    for g in &dropped {
        record(c, now, "group.role_remove", project, &roll, None, json!({ "group": g, "why": "the group was deleted while the project was" }))?;
    }
    c.execute("DELETE FROM deleted_group_roles WHERE project_id = ?", [project])?;
    Ok(dropped)
}

pub(crate) fn forget_aside(c: &Conn<'_>, project: &str) -> Result<()> {
    c.execute("DELETE FROM deleted_group_roles WHERE project_id = ?", [project])?;
    Ok(())
}
