//! Role, permission, user and identity statements, and what schema changes do to permissions.
//!
//! Each statement follows its reference's order of checks: Cassandra runs `authorize` (the
//! permission) then `validate` (options, login, existence) then `execute`
//! (`CreateRoleStatement`, `AlterRoleStatement`, `DropRoleStatement`, `GrantRoleStatement`,
//! `RevokeRoleStatement`, `GrantPermissionsStatement`, `RevokePermissionsStatement`,
//! `ListPermissionsStatement`, `ListRolesStatement`, `ListUsersStatement`,
//! `AddIdentityStatement`, `DropIdentityStatement`, with `CassandraRoleManager` /
//! `CassandraAuthorizer` writes). ScyllaDB checks the login first, reports permission failures
//! with the resource, words existence errors differently and keeps no permission index; its
//! order was taken from the auth-enabled reference.
//!
//! Writes go to the stored tables through internal CQL ([`store::apply`]); the snapshot is
//! reloaded afterwards.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use nosql_cql::ast::{
    self as ast, Permission, Permissions, RoleOption, Statement, StatementKind, UserPassword,
};
use nosql_types::{CqlType, Dialect};

use super::authorizer::{self as authz, Ctx, DcPermissions};
use super::messages as m;
use super::resource::{Resource, permission_from_name};
use super::store::{self, Snapshot, Write, p_bool, p_set, p_text};
use super::tables;
use crate::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::hooks::StatementHandler;
use crate::response::{
    ChangeTarget, ChangeType, ColumnSpec, QueryParams, Response, ResponseBody, ResultMetadata,
    Rows, SchemaChange,
};
use crate::schema::Schema;
use crate::session::SessionState;

/// The `StatementHandler` of the auth subsystem.
pub struct Handler;

impl StatementHandler for Handler {
    fn handles(&self, stmt: &StatementKind<'_>) -> bool {
        use StatementKind as S;
        matches!(
            stmt,
            S::Grant(_)
                | S::Revoke(_)
                | S::ListPermissions(_)
                | S::CreateRole(_)
                | S::AlterRole(_)
                | S::DropRole(_)
                | S::ListRoles(_)
                | S::GrantRole(_)
                | S::RevokeRole(_)
                | S::CreateUser(_)
                | S::AlterUser(_)
                | S::DropUser(_)
                | S::ListUsers
                | S::AddIdentity(_)
                | S::DropIdentity(_)
                | S::CreateServiceLevel(_)
                | S::AlterServiceLevel(_)
                | S::DropServiceLevel(_)
                | S::AttachServiceLevel(_)
                | S::DetachServiceLevel(_)
                | S::ListServiceLevels(_)
        )
    }

    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a mut SessionState,
        stmt: &'a Statement<'static>,
        _params: &'a QueryParams,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(execute(db, session, stmt))
    }
}

async fn execute(
    db: &Arc<Database>,
    session: &SessionState,
    stmt: &Statement<'static>,
) -> Result<Response> {
    use StatementKind as S;
    match &stmt.kind {
        S::CreateRole(r) => {
            create_role(
                db,
                session,
                &r.name.name(),
                r.if_exists,
                role_opts(&r.options),
            )
            .await
        }
        S::CreateUser(u) => {
            let mut o = user_opts(u);
            o.set("LOGIN", |o| o.login = Some(true));
            create_role(db, session, &u.name.name(), u.if_exists, o).await
        }
        S::AlterRole(r) => {
            alter_role(
                db,
                session,
                &r.name.name(),
                r.if_exists,
                role_opts(&r.options),
            )
            .await
        }
        S::AlterUser(u) => alter_role(db, session, &u.name.name(), u.if_exists, user_opts(u)).await,
        S::DropRole(r) | S::DropUser(r) => {
            drop_role(db, session, &r.name.name(), r.if_exists).await
        }
        S::GrantRole(g) => grant_role(db, session, &g.role.name(), &g.grantee.name(), true).await,
        S::RevokeRole(g) => grant_role(db, session, &g.role.name(), &g.grantee.name(), false).await,
        S::Grant(p) => permissions(db, session, p, true).await,
        S::Revoke(p) => permissions(db, session, p, false).await,
        S::ListPermissions(l) => list_permissions(db, session, l).await,
        S::ListRoles(l) => {
            list_roles(
                db,
                session,
                l.of.as_ref().map(|r| r.name().into_owned()),
                l.recursive,
            )
            .await
        }
        S::ListUsers => list_users(db, session).await,
        S::AddIdentity(a) => add_identity(db, session, a).await,
        S::DropIdentity(x) => drop_identity(db, session, x).await,
        _ => super::service_levels::execute(db, session, stmt).await,
    }
}

// ---------------------------------------------------------------------------------------------
// Role options
// ---------------------------------------------------------------------------------------------

/// `ACCESS TO DATACENTERS` / `ACCESS FROM CIDRS`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Access {
    All,
    Subset(Vec<String>),
}

/// The options of a role statement, with the order they were written in (validation order).
#[derive(Clone, Debug, Default)]
pub(crate) struct Opts {
    password: Option<String>,
    hashed: Option<String>,
    superuser: Option<bool>,
    login: Option<bool>,
    options: Option<Vec<(String, String)>>,
    dcs: Option<Access>,
    cidrs: Option<Access>,
    /// `IRoleManager.Option` names in written order.
    order: Vec<&'static str>,
}

impl Opts {
    fn set(&mut self, name: &'static str, f: impl FnOnce(&mut Opts)) {
        f(self);
        if !self.order.contains(&name) {
            self.order.push(name);
        }
    }

    /// No role-manager option (only network / CIDR access, or nothing).
    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

fn role_opts(options: &[RoleOption<'_>]) -> Opts {
    let mut o = Opts::default();
    for opt in options {
        match opt {
            RoleOption::Password(p) => o.set("PASSWORD", |o| o.password = Some(p.to_string())),
            RoleOption::HashedPassword(p) => {
                o.set("HASHED_PASSWORD", |o| o.hashed = Some(p.to_string()))
            }
            RoleOption::Superuser(b) => o.set("SUPERUSER", |o| o.superuser = Some(*b)),
            RoleOption::Login(b) => o.set("LOGIN", |o| o.login = Some(*b)),
            RoleOption::Options(m) => o.set("OPTIONS", |o| {
                o.options = Some(
                    m.iter()
                        .map(|(k, v)| (k.text.to_string(), v.text.to_string()))
                        .collect(),
                )
            }),
            RoleOption::AccessToAllDatacenters => o.dcs = Some(Access::All),
            RoleOption::AccessToDatacenters(d) => {
                o.dcs = Some(Access::Subset(d.iter().map(|x| x.to_string()).collect()))
            }
            RoleOption::AccessFromAllCidrs => o.cidrs = Some(Access::All),
            RoleOption::AccessFromCidrs(c) => {
                o.cidrs = Some(Access::Subset(c.iter().map(|x| x.to_string()).collect()))
            }
        }
    }
    o
}

fn user_opts(u: &ast::UserStatement<'_>) -> Opts {
    let mut o = Opts::default();
    match &u.password {
        Some(UserPassword::Plain(p)) => o.set("PASSWORD", |o| o.password = Some(p.to_string())),
        Some(UserPassword::Hashed(p)) => {
            o.set("HASHED_PASSWORD", |o| o.hashed = Some(p.to_string()))
        }
        None => {}
    }
    if let Some(s) = u.superuser {
        o.set("SUPERUSER", |o| o.superuser = Some(s));
    }
    o
}

/// `RoleOptions.validate` (Cassandra): options the role manager supports, bcrypt hashes.
fn validate_opts(cx: &Ctx, o: &Opts) -> Result<()> {
    for name in &o.order {
        let supported = match *name {
            "LOGIN" | "SUPERUSER" => true,
            "PASSWORD" | "HASHED_PASSWORD" => cx.authn,
            _ => false,
        };
        if !supported {
            return Err(m::unsupported_option(name));
        }
        if *name == "HASHED_PASSWORD"
            && !o
                .hashed
                .as_deref()
                .is_some_and(super::hash::jbcrypt_salt_ok)
        {
            return Err(m::invalid_bcrypt());
        }
    }
    Ok(())
}

/// `DCPermissions.validate` / `CIDRPermissions.validate`.
fn validate_access(db: &Database, o: &Opts) -> Result<()> {
    if let Some(Access::Subset(dcs)) = &o.dcs
        && dcs.iter().any(|d| *d != db.cfg.datacenter)
    {
        return Err(m::invalid_datacenters(dcs));
    }
    if let Some(Access::Subset(groups)) = &o.cidrs {
        // roll has no CIDR groups (`nodetool updatecidrgroup` doesn't exist here).
        return Err(m::invalid_cidr_groups(groups));
    }
    Ok(())
}

/// A role-statement permission check in the dialect's wording.
fn role_perm(cx: &Ctx, perm: Permission, r: &Resource) -> Result<()> {
    match cx.ensure(perm, r) {
        Err(e) if e.code == ErrorCode::Unauthorized && cx.d == Dialect::Cassandra => {
            Err(m::insufficient_privileges(cx.user_name()))
        }
        other => other,
    }
}

fn roles_ks(d: Dialect) -> &'static str {
    tables::names(d).keyspace
}

fn void() -> Result<Response> {
    Ok(Response::void())
}

fn now_micros(db: &Database) -> i64 {
    db.timestamp()
        + db.auth
            .clock_skew_us
            .load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------------------------
// CREATE / ALTER / DROP ROLE
// ---------------------------------------------------------------------------------------------

async fn create_role(
    db: &Arc<Database>,
    session: &SessionState,
    role: &str,
    if_not_exists: bool,
    o: Opts,
) -> Result<Response> {
    let (_g, snap) = store::lock(db).await?;
    let cx = Ctx::new(db, session);
    let d = cx.d;
    match d {
        Dialect::Cassandra => {
            role_perm(&cx, Permission::Create, &Resource::RolesRoot)?;
            if o.superuser == Some(true) && !cx.user_is_super() {
                return Err(m::create_superuser_only(d));
            }
            validate_opts(&cx, &o)?;
            if role.is_empty() {
                return Err(m::empty_role_name(d));
            }
            validate_access(db, &o)?;
            cx.ensure_not_anonymous()?;
        }
        Dialect::Scylla => {
            cx.ensure_not_anonymous()?;
            if cx.enforce && !cx.user_is_super() {
                cx.ensure(Permission::Create, &Resource::RolesRoot)?;
            }
            if o.superuser == Some(true) && !cx.user_is_super() {
                return Err(m::create_superuser_only(d));
            }
            if o.hashed.is_some() && !cx.user_is_super() {
                return Err(m::scylla_hashed_superuser_only());
            }
            if o.options.is_some() {
                return Err(m::scylla_options_unsupported());
            }
            if o.password.is_some() && o.hashed.is_some() {
                return Err(m::scylla_password_and_hashed());
            }
            if role.is_empty() {
                return Err(m::empty_role_name(d));
            }
        }
    }
    if snap.exists(role) {
        if if_not_exists {
            return void();
        }
        return Err(m::role_exists(d, role));
    }
    let ks = roles_ks(d);
    let mut writes = Vec::new();
    let hash = match (&o.password, &o.hashed) {
        (_, Some(h)) => Some(h.clone()),
        (Some(p), None) => Some(super::hash_password(db, p).await),
        (None, None) => None,
    };
    let (sup, login) = (o.superuser.unwrap_or(false), o.login.unwrap_or(false));
    writes.push(match &hash {
        Some(h) => Write::new(
            format!("INSERT INTO {ks}.roles (role, is_superuser, can_login, salted_hash) VALUES (?, ?, ?, ?)"),
            vec![p_text(role), p_bool(sup), p_bool(login), p_text(h)],
        ),
        None => Write::new(
            format!("INSERT INTO {ks}.roles (role, is_superuser, can_login) VALUES (?, ?, ?)"),
            vec![p_text(role), p_bool(sup), p_bool(login)],
        ),
    });
    if d == Dialect::Cassandra {
        if authz::network_enforcing(db) {
            writes.push(network_write(role, o.dcs.as_ref().unwrap_or(&Access::All)));
        }
        writes.push(cidr_write(role, o.cidrs.as_ref().unwrap_or(&Access::All)));
    }
    store::apply(db, writes, None).await?;
    if hash.is_some() {
        db.auth
            .hash_written
            .lock()
            .insert(role.to_string(), now_micros(db));
    }
    if cx.enforce
        && let Some(creator) = cx.user
    {
        let r = Resource::Role(role.to_string());
        let snap = store::reload(db).await?;
        let perms: Vec<Permission> = r.applicable(d).to_vec();
        store::apply(db, grant_writes(d, &snap, creator, &r, &perms).0, None).await?;
    }
    store::reload(db).await?;
    void()
}

fn network_write(role: &str, a: &Access) -> Write {
    let dcs: Vec<&str> = match a {
        Access::All => Vec::new(),
        Access::Subset(s) => s.iter().map(String::as_str).collect(),
    };
    Write::new(
        "UPDATE system_auth.network_permissions SET dcs = ? WHERE role = ?",
        vec![p_set(dcs), p_text(&format!("roles/{role}"))],
    )
}

fn cidr_write(role: &str, a: &Access) -> Write {
    let groups: Vec<&str> = match a {
        Access::All => Vec::new(),
        Access::Subset(s) => s.iter().map(String::as_str).collect(),
    };
    Write::new(
        "UPDATE system_auth.cidr_permissions SET cidr_groups = ? WHERE role = ?",
        vec![p_set(groups), p_text(role)],
    )
}

async fn alter_role(
    db: &Arc<Database>,
    session: &SessionState,
    role: &str,
    if_exists: bool,
    o: Opts,
) -> Result<Response> {
    let (_g, snap) = store::lock(db).await?;
    let cx = Ctx::new(db, session);
    let d = cx.d;
    let is_super = cx.user_is_super();
    let own = cx.user == Some(role);
    match d {
        Dialect::Cassandra => {
            if o.superuser.is_some() && cx.user_role_names().contains(role) {
                return Err(m::alter_own_superuser(d));
            }
            if o.superuser.is_some() && !is_super {
                return Err(m::alter_superuser_only(d));
            }
            if o.dcs.is_some() && !is_super {
                return Err(m::alter_dcs_superuser_only());
            }
            if o.cidrs.is_some() && !is_super {
                return Err(m::alter_cidrs_superuser_only());
            }
            if !is_super {
                if own {
                    for name in &o.order {
                        if !(cx.authn && matches!(*name, "PASSWORD" | "HASHED_PASSWORD")) {
                            return Err(m::alter_own_option(name));
                        }
                    }
                } else {
                    role_perm(&cx, Permission::Alter, &Resource::Role(role.to_string()))?;
                }
            }
            validate_opts(&cx, &o)?;
            if o.is_empty() && o.dcs.is_none() && o.cidrs.is_none() {
                return Err(m::alter_empty());
            }
            validate_access(db, &o)?;
            cx.ensure_not_anonymous()?;
            if !snap.exists(role) {
                if if_exists {
                    return void();
                }
                return Err(m::role_missing_prefixed(d, role));
            }
        }
        Dialect::Scylla => {
            cx.ensure_not_anonymous()?;
            if o.superuser.is_some() {
                if !is_super {
                    return Err(m::alter_superuser_only(d));
                }
                if cx.user_role_names().contains(role) {
                    return Err(m::alter_own_superuser(d));
                }
            }
            if !is_super {
                if own {
                    if o.options.is_some() {
                        return Err(m::scylla_alter_own_options());
                    }
                } else if cx.enforce {
                    cx.ensure(Permission::Alter, &Resource::Role(role.to_string()))?;
                }
            }
            if !snap.exists(role) {
                return Err(m::role_missing_prefixed(d, role));
            }
            if o.options.is_some() {
                return Err(m::scylla_options_unsupported());
            }
            if o.hashed.is_some() {
                return Err(m::scylla_alter_hashed());
            }
        }
    }
    let ks = roles_ks(d);
    let mut sets: Vec<&str> = Vec::new();
    let mut values = Vec::new();
    if o.password.is_some() && d == Dialect::Cassandra {
        let interval = db.settings().role_password_update_min_interval_ms;
        if interval > 0
            && let Some(at) = db
                .auth
                .hash_written
                .lock()
                .get(role)
                .copied()
                .or_else(|| snap.roles.get(role).and_then(|r| r.hash_written))
            && now_micros(db) - at < (interval as i64) * 1000
        {
            return Err(m::password_rate_limited(role, interval));
        }
    }
    for name in &o.order {
        match *name {
            "LOGIN" => {
                sets.push("can_login = ?");
                values.push(p_bool(o.login.unwrap_or(false)));
            }
            "SUPERUSER" => {
                sets.push("is_superuser = ?");
                values.push(p_bool(o.superuser.unwrap_or(false)));
            }
            "PASSWORD" => {
                let h = super::hash_password(db, o.password.as_deref().unwrap_or("")).await;
                sets.push("salted_hash = ?");
                values.push(p_text(&h));
            }
            "HASHED_PASSWORD" => {
                sets.push("salted_hash = ?");
                values.push(p_text(o.hashed.as_deref().unwrap_or("")));
            }
            _ => {}
        }
    }
    if o.password.is_some() || o.hashed.is_some() {
        db.auth
            .hash_written
            .lock()
            .insert(role.to_string(), now_micros(db));
    }
    if !sets.is_empty() {
        values.push(p_text(role));
        store::apply(
            db,
            vec![Write::new(
                format!("UPDATE {ks}.roles SET {} WHERE role = ?", sets.join(", ")),
                values,
            )],
            None,
        )
        .await?;
    }
    if d == Dialect::Cassandra {
        if let Some(dcs) = &o.dcs {
            if !authz::network_enforcing(db) {
                store::reload(db).await?;
                return Err(m::dcs_not_supported());
            }
            store::apply(db, vec![network_write(role, dcs)], None).await?;
        }
        if let Some(c) = &o.cidrs {
            store::apply(db, vec![cidr_write(role, c)], None).await?;
        }
    }
    store::reload(db).await?;
    void()
}

async fn drop_role(
    db: &Arc<Database>,
    session: &SessionState,
    role: &str,
    if_exists: bool,
) -> Result<Response> {
    let (_g, snap) = store::lock(db).await?;
    let cx = Ctx::new(db, session);
    let d = cx.d;
    let r = Resource::Role(role.to_string());
    let exists = snap.exists(role);
    match d {
        Dialect::Cassandra => {
            role_perm(&cx, Permission::Drop, &r)?;
            if exists && snap.is_super(role) && !cx.user_is_super() {
                return Err(m::drop_superuser_only(d));
            }
            cx.ensure_not_anonymous()?;
            if !if_exists && !exists {
                return Err(m::role_missing(d, role));
            }
            if cx.user == Some(role) {
                return Err(m::drop_primary_role(d));
            }
        }
        Dialect::Scylla => {
            cx.ensure_not_anonymous()?;
            if cx.enforce && !cx.user_is_super() {
                cx.ensure(Permission::Drop, &r)?;
            }
            if exists && snap.is_super(role) && !cx.user_is_super() {
                return Err(m::drop_superuser_only(d));
            }
            if !if_exists && !exists {
                return Err(m::role_missing(d, role));
            }
            if cx.user == Some(role) {
                return Err(m::drop_primary_role(d));
            }
        }
    }
    if !exists {
        return void();
    }
    let ks = roles_ks(d);
    let mut w = Vec::new();
    // Members of the dropped role lose it.
    for member in snap.members.get(role).into_iter().flatten() {
        w.push(Write::new(
            format!("UPDATE {ks}.roles SET member_of = member_of - ? WHERE role = ?"),
            vec![p_set([role]), p_text(member)],
        ));
    }
    w.push(Write::new(
        format!("DELETE FROM {ks}.role_members WHERE role = ?"),
        vec![p_text(role)],
    ));
    match d {
        Dialect::Cassandra => {
            for (identity, owner) in &snap.identities {
                if owner == role {
                    w.push(Write::new(
                        "DELETE FROM system_auth.identity_to_role WHERE identity = ?",
                        vec![p_text(identity)],
                    ));
                }
            }
            if authz::network_enforcing(db) {
                w.push(Write::new(
                    "DELETE FROM system_auth.network_permissions WHERE role = ?",
                    vec![p_text(&format!("roles/{role}"))],
                ));
            }
            w.push(Write::new(
                "DELETE FROM system_auth.cidr_permissions WHERE role = ?",
                vec![p_text(role)],
            ));
        }
        Dialect::Scylla => {
            // ScyllaDB also removes the dropped role from the roles it was granted.
            if let Some(row) = snap.roles.get(role) {
                for parent in &row.member_of {
                    w.push(Write::new(
                        "DELETE FROM system.role_members WHERE role = ? AND member = ?",
                        vec![p_text(parent), p_text(role)],
                    ));
                }
            }
            w.push(Write::new(
                "DELETE FROM system.role_attributes WHERE role = ?",
                vec![p_text(role)],
            ));
        }
    }
    w.push(Write::new(
        format!("DELETE FROM {ks}.roles WHERE role = ?"),
        vec![p_text(role)],
    ));
    if cx.enforce {
        w.extend(revoke_all_from(d, &snap, role));
        w.extend(revoke_all_on(d, &snap, &r.name()));
    }
    store::apply(db, w, None).await?;
    store::reload(db).await?;
    void()
}

/// `CassandraAuthorizer.revokeAllFrom`: every permission of `role`.
fn revoke_all_from(d: Dialect, snap: &Snapshot, role: &str) -> Vec<Write> {
    let ks = roles_ks(d);
    let mut w = Vec::new();
    if d == Dialect::Cassandra {
        for res in snap
            .permissions
            .get(role)
            .into_iter()
            .flat_map(|m| m.keys())
        {
            w.push(Write::new(
                "DELETE FROM system_auth.resource_role_permissons_index WHERE resource = ? AND role = ?",
                vec![p_text(res), p_text(role)],
            ));
        }
    }
    w.push(Write::new(
        format!("DELETE FROM {ks}.role_permissions WHERE role = ?"),
        vec![p_text(role)],
    ));
    w
}

/// `CassandraAuthorizer.revokeAllOn`: every permission on a resource (Cassandra finds the roles
/// through its index, ScyllaDB through the permissions themselves).
fn revoke_all_on(d: Dialect, snap: &Snapshot, resource: &str) -> Vec<Write> {
    let ks = roles_ks(d);
    let mut w = Vec::new();
    let roles: Vec<String> = match d {
        Dialect::Cassandra => snap
            .index
            .get(resource)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default(),
        Dialect::Scylla => snap
            .permissions
            .iter()
            .filter(|(_, m)| m.contains_key(resource))
            .map(|(r, _)| r.clone())
            .collect(),
    };
    for role in roles {
        w.push(Write::new(
            format!("DELETE FROM {ks}.role_permissions WHERE role = ? AND resource = ?"),
            vec![p_text(&role), p_text(resource)],
        ));
    }
    if d == Dialect::Cassandra {
        w.push(Write::new(
            "DELETE FROM system_auth.resource_role_permissons_index WHERE resource = ?",
            vec![p_text(resource)],
        ));
    }
    w
}

// ---------------------------------------------------------------------------------------------
// GRANT / REVOKE role
// ---------------------------------------------------------------------------------------------

async fn grant_role(
    db: &Arc<Database>,
    session: &SessionState,
    role: &str,
    grantee: &str,
    grant: bool,
) -> Result<Response> {
    let (_g, snap) = store::lock(db).await?;
    let cx = Ctx::new(db, session);
    let d = cx.d;
    let r = Resource::Role(role.to_string());
    match d {
        Dialect::Cassandra => {
            role_perm(&cx, Permission::Authorize, &r)?;
            cx.ensure_not_anonymous()?;
        }
        Dialect::Scylla => {
            cx.ensure_not_anonymous()?;
            if cx.enforce && !cx.user_is_super() {
                cx.ensure(Permission::Authorize, &r)?;
            }
        }
    }
    if !snap.exists(role) {
        return Err(m::role_missing(d, role));
    }
    if !snap.exists(grantee) {
        return Err(m::role_missing(d, grantee));
    }
    let ks = roles_ks(d);
    let w = if grant {
        if snap.roles_of(grantee, true).contains(role) {
            return Err(m::already_member(d, grantee, role));
        }
        if snap.roles_of(role, true).contains(grantee) {
            return Err(m::already_member(d, role, grantee));
        }
        vec![
            Write::new(
                format!("UPDATE {ks}.roles SET member_of = member_of + ? WHERE role = ?"),
                vec![p_set([role]), p_text(grantee)],
            ),
            Write::new(
                format!("INSERT INTO {ks}.role_members (role, member) VALUES (?, ?)"),
                vec![p_text(role), p_text(grantee)],
            ),
        ]
    } else {
        if !snap.roles_of(grantee, false).contains(role) {
            return Err(m::not_member(d, grantee, role));
        }
        vec![
            Write::new(
                format!("UPDATE {ks}.roles SET member_of = member_of - ? WHERE role = ?"),
                vec![p_set([role]), p_text(grantee)],
            ),
            Write::new(
                format!("DELETE FROM {ks}.role_members WHERE role = ? AND member = ?"),
                vec![p_text(role), p_text(grantee)],
            ),
        ]
    };
    store::apply(db, w, None).await?;
    store::reload(db).await?;
    void()
}

// ---------------------------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------------------------

/// Resolves a resource of a statement (`AuthorizationStatement.maybeCorrectResource` for
/// unqualified tables). Functions are resolved against the schema (types, existence for
/// Cassandra's `applicablePermissions` assertion).
fn resolve_resource(
    db: &Database,
    session: &SessionState,
    r: &ast::Resource<'_>,
) -> Result<Resource> {
    let d = db.dialect();
    let current = || {
        session
            .keyspace
            .clone()
            .ok_or_else(crate::messages::no_keyspace)
    };
    Ok(match r {
        ast::Resource::AllKeyspaces => Resource::DataRoot,
        ast::Resource::Keyspace(k) => Resource::Keyspace(k.name().into_owned()),
        ast::Resource::AllTablesInKeyspace(k) => Resource::AllTables(k.name().into_owned()),
        ast::Resource::Table { name, .. } => Resource::Table(
            match &name.keyspace {
                Some(k) => k.name().into_owned(),
                None => current()?,
            },
            name.name.name().into_owned(),
        ),
        ast::Resource::AllRoles => Resource::RolesRoot,
        ast::Resource::Role(r) => Resource::Role(r.name().into_owned()),
        ast::Resource::AllFunctions => Resource::FunctionsRoot,
        ast::Resource::AllFunctionsInKeyspace(k) => Resource::FunctionsIn(k.name().into_owned()),
        ast::Resource::Function { name, arg_types } => {
            let ks = match &name.keyspace {
                Some(k) => k.name().into_owned(),
                None => current()?,
            };
            if ks == "system" {
                return Err(m::builtin_function_permissions());
            }
            let schema = db.schema();
            let udts = schema
                .keyspace(&ks)
                .map(|k| k.types.clone())
                .unwrap_or_default();
            let mut types: Vec<CqlType> = Vec::new();
            for t in arg_types {
                let ty = crate::schema::types::resolve(t, &ks, &|n| udts.get(n).cloned(), d)?;
                // ScyllaDB's function signatures keep user types frozen (`udf::ddl`).
                types.push(match d {
                    Dialect::Scylla => crate::udf::ddl::scylla_freeze_udt(ty),
                    Dialect::Cassandra => ty,
                });
            }
            Resource::Function {
                keyspace: ks,
                name: name.resolved().into_owned(),
                marshal: types
                    .iter()
                    .map(|t| nosql_types::marshal_name(t, false, d))
                    .collect(),
                cql: types.iter().map(nosql_types::cql_name).collect(),
            }
        }
        ast::Resource::AllMbeans => Resource::MbeansRoot,
        ast::Resource::Mbean { name, .. } => Resource::Mbean(name.to_string()),
    })
}

/// `IResource.exists`. JMX names can't be looked up here: patterns count as existing, single
/// names don't (the reference has no MBean named by a test).
fn resource_exists(db: &Database, snap: &Snapshot, r: &Resource) -> bool {
    let schema = db.schema();
    match r {
        Resource::DataRoot
        | Resource::RolesRoot
        | Resource::FunctionsRoot
        | Resource::MbeansRoot
        | Resource::ServiceLevels => true,
        Resource::Keyspace(k) | Resource::AllTables(k) | Resource::FunctionsIn(k) => {
            schema.keyspace(k).is_some()
        }
        Resource::Table(k, t) => schema
            .keyspace(k)
            .is_some_and(|ks| ks.tables.contains_key(t)),
        Resource::Role(r) => snap.exists(r),
        Resource::Function { .. } => function_exists(&schema, r, db.dialect()),
        Resource::Mbean(n) => n.contains('*') || n.contains('?'),
    }
}

fn function_exists(schema: &Schema, r: &Resource, d: Dialect) -> bool {
    let Resource::Function {
        keyspace,
        name,
        cql,
        ..
    } = r
    else {
        return false;
    };
    let Some(ks) = schema.keyspace(keyspace) else {
        return false;
    };
    // Cassandra's `findUserFunction`: the frozen types match (`list<int>` names
    // `frozen<list<int>>` too); ScyllaDB compares the signatures as stored.
    let types: Option<Vec<CqlType>> = cql
        .iter()
        .map(|t| crate::udf::function::parse_type(ks, t))
        .collect();
    let same = |texts: &[String]| match (&types, d) {
        (Some(types), Dialect::Cassandra) => {
            crate::udf::function::types_match_texts(ks, texts, types)
        }
        _ => texts == cql.as_slice(),
    };
    ks.functions
        .iter()
        .any(|f| f.name == *name && same(&f.argument_types))
        || ks
            .aggregates
            .iter()
            .any(|a| a.name == *name && same(&a.argument_types))
}

/// The permissions a GRANT / REVOKE names, kept to those that apply (`filterPermissions`).
fn requested(p: &Permissions, r: &Resource, d: Dialect) -> Vec<Permission> {
    let applicable = r.applicable(d);
    p.set()
        .into_iter()
        .filter(|x| applicable.contains(x))
        .collect()
}

/// The writes granting `perms` on `r` to `role`, and the permissions it already had.
fn grant_writes(
    d: Dialect,
    snap: &Snapshot,
    role: &str,
    r: &Resource,
    perms: &[Permission],
) -> (Vec<Write>, Vec<Permission>) {
    let name = r.name();
    let have = snap.granted(role, &name);
    let (existing, new): (Vec<Permission>, Vec<Permission>) =
        perms.iter().partition(|p| have.contains(p.as_str()));
    let mut w = Vec::new();
    if !new.is_empty() {
        let ks = roles_ks(d);
        w.push(Write::new(
            format!(
                "UPDATE {ks}.role_permissions SET permissions = permissions + ? WHERE role = ? AND resource = ?"
            ),
            vec![
                p_set(new.iter().map(|p| p.as_str())),
                p_text(role),
                p_text(&name),
            ],
        ));
        if d == Dialect::Cassandra {
            w.push(Write::new(
                "INSERT INTO system_auth.resource_role_permissons_index (resource, role) VALUES (?, ?)",
                vec![p_text(&name), p_text(role)],
            ));
        }
    }
    (w, existing)
}

fn join_perms(perms: &[Permission]) -> String {
    perms
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Cassandra's system keyspaces (not the virtual ones) for `GrantPermissionsStatement`.
fn non_virtual_system(ks: &str) -> bool {
    matches!(
        ks,
        "system" | "system_schema" | "system_auth" | "system_distributed" | "system_traces"
    )
}

async fn permissions(
    db: &Arc<Database>,
    session: &SessionState,
    p: &ast::PermissionsStatement<'_>,
    grant: bool,
) -> Result<Response> {
    let d = db.dialect();
    let r = resolve_resource(db, session, &p.resource)?;
    let (_g, snap) = store::lock(db).await?;
    if d == Dialect::Cassandra
        && matches!(r, Resource::Function { .. })
        && !function_exists(&db.schema(), &r, d)
    {
        return Err(m::cassandra_missing_function(&r.display()));
    }
    let perms = requested(&p.permissions, &r, d);
    if perms.is_empty() {
        // Only function resources reach here (the parser filters the others).
        return Err(Error::syntax(
            "Resource type FunctionResource does not support any of the requested permissions",
        ));
    }
    let grantee = p.role.name().into_owned();
    let cx = Ctx::new(db, session);
    match d {
        Dialect::Cassandra => {
            cx.ensure(Permission::Authorize, &r)?;
            for x in &perms {
                cx.ensure(*x, &r)?;
            }
            cx.ensure_not_anonymous()?;
            if !snap.exists(&grantee) {
                return Err(m::role_missing_prefixed(d, &grantee));
            }
            if !resource_exists(db, &snap, &r) {
                return Err(m::resource_missing(d, &r.display_in(d)));
            }
            if grant
                && r.data_keyspace().is_some_and(non_virtual_system)
                && perms.iter().any(|x| {
                    !matches!(
                        x,
                        Permission::Select | Permission::Describe | Permission::Alter
                    )
                })
            {
                return Err(m::system_keyspace_grant());
            }
        }
        Dialect::Scylla => {
            cx.ensure_not_anonymous()?;
            if cx.enforce && !cx.user_is_super() {
                cx.ensure(Permission::Authorize, &r)?;
                for x in &perms {
                    cx.ensure(*x, &r)?;
                }
            }
            if !resource_exists(db, &snap, &r) {
                return Err(m::resource_missing(d, &r.display_in(d)));
            }
            if !snap.exists(&grantee) {
                return Err(m::role_missing_prefixed(d, &grantee));
            }
        }
    }
    if !cx.enforce {
        return Err(m::allow_all_unsupported(if grant {
            "GRANT"
        } else {
            "REVOKE"
        }));
    }
    let mut resp = Response::void();
    if grant {
        let (w, existing) = grant_writes(d, &snap, &grantee, &r, &perms);
        store::apply(db, w, None).await?;
        if d == Dialect::Cassandra && !existing.is_empty() {
            resp.warnings.push(m::already_granted(
                &grantee,
                &join_perms(&existing),
                &r.display(),
            ));
        }
    } else {
        let name = r.name();
        let have = snap.granted(&grantee, &name);
        let (revoked, missing): (Vec<Permission>, Vec<Permission>) =
            perms.iter().partition(|x| have.contains(x.as_str()));
        if !revoked.is_empty() {
            let ks = roles_ks(d);
            let mut w = vec![Write::new(
                format!(
                    "UPDATE {ks}.role_permissions SET permissions = permissions - ? WHERE role = ? AND resource = ?"
                ),
                vec![
                    p_set(revoked.iter().map(|p| p.as_str())),
                    p_text(&grantee),
                    p_text(&name),
                ],
            )];
            if d == Dialect::Cassandra {
                // As `CassandraAuthorizer.revoke`: the index entry goes even if other
                // permissions on the resource remain.
                w.push(Write::new(
                    "DELETE FROM system_auth.resource_role_permissons_index WHERE resource = ? AND role = ?",
                    vec![p_text(&name), p_text(&grantee)],
                ));
            }
            store::apply(db, w, None).await?;
        }
        if d == Dialect::Cassandra && !missing.is_empty() {
            resp.warnings.push(m::not_granted(
                &grantee,
                &join_perms(&missing),
                &r.display(),
            ));
        }
    }
    store::reload(db).await?;
    Ok(resp)
}

// ---------------------------------------------------------------------------------------------
// LIST statements
// ---------------------------------------------------------------------------------------------

fn spec(d: Dialect, table: &str, name: &str, ty: CqlType) -> ColumnSpec {
    let ks = if d.is_scylla() {
        "system"
    } else {
        "system_auth"
    };
    ColumnSpec::new(ks, table, name, ty)
}

fn rows(columns: Vec<ColumnSpec>, rows: Vec<Vec<Option<Bytes>>>) -> Response {
    Response::new(ResponseBody::Rows(Rows {
        metadata: ResultMetadata { columns },
        rows,
        ..Default::default()
    }))
}

fn b_text(s: &str) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(s.as_bytes()))
}

fn b_bool(b: bool) -> Option<Bytes> {
    Some(Bytes::from_static(if b { &[1] } else { &[0] }))
}

fn empty_map() -> Option<Bytes> {
    Some(Bytes::from_static(&[0, 0, 0, 0]))
}

async fn list_permissions(
    db: &Arc<Database>,
    session: &SessionState,
    l: &ast::ListPermissions<'_>,
) -> Result<Response> {
    let d = db.dialect();
    let snap = store::ensure_fresh(db).await?;
    let cx = Ctx::new(db, session);
    let resource = match &l.resource {
        Some(r) => Some(resolve_resource(db, session, r)?),
        None => None,
    };
    let grantee = l.of.as_ref().map(|r| r.name().into_owned());
    match d {
        Dialect::Cassandra => {
            cx.ensure_not_anonymous()?;
            if let Some(r) = &resource
                && !resource_exists(db, &snap, r)
            {
                return Err(m::list_resource_missing(d, &r.display_in(d)));
            }
            if let Some(g) = &grantee
                && !snap.exists(g)
            {
                return Err(m::list_role_missing(d, g));
            }
            if !cx.enforce {
                return Err(m::allow_all_list());
            }
            // `CassandraAuthorizer.list`: own roles, DESCRIBE on all roles or on the grantee.
            let allowed = cx.user_is_super()
                || grantee
                    .as_ref()
                    .is_some_and(|g| cx.user_role_names().contains(g))
                || cx
                    .permissions_on(&Resource::RolesRoot)
                    .contains(&Permission::Describe)
                || grantee.as_ref().is_some_and(|g| {
                    cx.permissions_on(&Resource::Role(g.clone()))
                        .contains(&Permission::Describe)
                });
            if !allowed {
                return Err(m::view_permissions_of(
                    grantee.as_deref().unwrap_or("everyone"),
                ));
            }
        }
        Dialect::Scylla => {
            if resource.is_none() && grantee.is_none() && !cx.user_is_super() {
                return Err(m::view_permissions_of("everyone"));
            }
            cx.ensure_not_anonymous()?;
            if let Some(r) = &resource
                && !resource_exists(db, &snap, r)
            {
                return Err(m::list_resource_missing(d, &r.display_in(d)));
            }
            if let Some(g) = &grantee {
                if !snap.exists(g) {
                    return Err(m::list_role_missing(d, g));
                }
                if !cx.user_is_super() && !cx.user_role_names().contains(g) {
                    return Err(m::view_permissions_of(g));
                }
            }
        }
    }
    let wanted: BTreeSet<Permission> = l.permissions.set().into_iter().collect();
    let chain: Vec<Option<Resource>> = match &resource {
        Some(r) if l.recursive => r.chain(d).into_iter().map(Some).collect(),
        Some(r) => vec![Some(r.clone())],
        None => vec![None],
    };
    let roles: Vec<String> = match &grantee {
        Some(g) => snap.roles_of(g, true).into_iter().collect(),
        None => snap.permissions.keys().cloned().collect(),
    };
    let mut details: BTreeSet<(String, u8, String, usize, String, Permission)> = BTreeSet::new();
    for role in &roles {
        let Some(granted) = snap.permissions.get(role) else {
            continue;
        };
        for (res_name, perms) in granted {
            let Some(res) = Resource::from_name(res_name) else {
                continue;
            };
            if !chain.iter().any(|c| c.as_ref().is_none_or(|c| *c == res)) {
                continue;
            }
            for p in perms {
                let Some(p) = permission_from_name(p) else {
                    continue;
                };
                if !wanted.contains(&p) {
                    continue;
                }
                let (kind, order) = match d {
                    // Grantee, resource name, permission declaration order.
                    Dialect::Cassandra => (0, p as usize),
                    // Grantee, resource kind and name, permission name.
                    Dialect::Scylla => (res.kind_order(), 0),
                };
                let key_perm = if d.is_scylla() {
                    p.as_str().to_string()
                } else {
                    String::new()
                };
                details.insert((role.clone(), kind, res_name.clone(), order, key_perm, p));
            }
        }
    }
    if details.is_empty() && d == Dialect::Cassandra {
        return void();
    }
    let out = details
        .into_iter()
        .map(|(role, _, res_name, _, _, p)| {
            let display = Resource::from_name(&res_name)
                .map(|r| r.display_in(d))
                .unwrap_or(res_name);
            vec![
                b_text(&role),
                b_text(&role),
                b_text(&display),
                b_text(p.as_str()),
            ]
        })
        .collect();
    let cols = ["role", "username", "resource", "permission"]
        .iter()
        .map(|c| spec(d, "permissions", c, CqlType::Text))
        .collect();
    Ok(rows(cols, out))
}

/// The roles a LIST ROLES / LIST USERS shows (`ListRolesStatement.execute`).
fn visible_roles(
    cx: &Ctx,
    snap: &Snapshot,
    grantee: Option<&str>,
    recursive: bool,
) -> Result<BTreeSet<String>> {
    let d = cx.d;
    let describe = cx.user_is_super()
        || !cx.enforce
        || cx
            .permissions_on(&Resource::RolesRoot)
            .contains(&Permission::Describe);
    if describe {
        return Ok(match grantee {
            None => snap.roles.keys().cloned().collect(),
            Some(g) => snap.roles_of(g, recursive),
        });
    }
    let user = cx.user.unwrap_or_default();
    if d.is_scylla() && !snap.exists(user) {
        return Err(m::list_role_missing(d, user));
    }
    match grantee {
        None => Ok(snap.roles_of(user, recursive)),
        Some(g) if snap.roles_of(user, true).contains(g) => Ok(snap.roles_of(g, recursive)),
        Some(g) => Err(m::view_roles_of(d, g)),
    }
}

async fn list_roles(
    db: &Arc<Database>,
    session: &SessionState,
    grantee: Option<String>,
    recursive: bool,
) -> Result<Response> {
    let d = db.dialect();
    let snap = store::ensure_fresh(db).await?;
    let cx = Ctx::new(db, session);
    cx.ensure_not_anonymous()?;
    if let Some(g) = &grantee
        && !snap.exists(g)
    {
        return Err(m::list_role_missing(d, g));
    }
    let roles = visible_roles(&cx, &snap, grantee.as_deref(), recursive)?;
    if roles.is_empty() && d == Dialect::Cassandra {
        return void();
    }
    let options_ty = CqlType::map(CqlType::Text, CqlType::Text, false);
    let mut cols = vec![
        spec(d, "roles", "role", CqlType::Text),
        spec(d, "roles", "super", CqlType::Boolean),
        spec(d, "roles", "login", CqlType::Boolean),
        spec(d, "roles", "options", options_ty),
    ];
    if d == Dialect::Cassandra {
        cols.push(spec(d, "roles", "datacenters", CqlType::Text));
    }
    let out = roles
        .iter()
        .filter_map(|r| snap.roles.get(r))
        .map(|r| {
            let mut row = vec![
                b_text(&r.name),
                b_bool(r.is_superuser),
                b_bool(r.can_login),
                empty_map(),
            ];
            if d == Dialect::Cassandra {
                row.push(b_text(&authz::dc_permissions(db, &snap, &r.name).text()));
            }
            row
        })
        .collect();
    Ok(rows(cols, out))
}

async fn list_users(db: &Arc<Database>, session: &SessionState) -> Result<Response> {
    let d = db.dialect();
    let snap = store::ensure_fresh(db).await?;
    let cx = Ctx::new(db, session);
    cx.ensure_not_anonymous()?;
    let roles = match d {
        Dialect::Cassandra => visible_roles(&cx, &snap, None, false)?,
        // ScyllaDB lists only the user's own role to non-superusers.
        Dialect::Scylla if !cx.user_is_super() => cx
            .user
            .map(|u| BTreeSet::from([u.to_string()]))
            .unwrap_or_default(),
        Dialect::Scylla => snap.roles.keys().cloned().collect(),
    };
    if roles.is_empty() && d == Dialect::Cassandra {
        return void();
    }
    let mut cols = vec![
        spec(d, "users", "name", CqlType::Text),
        spec(d, "users", "super", CqlType::Boolean),
    ];
    if d == Dialect::Cassandra {
        cols.push(spec(d, "users", "datacenters", CqlType::Text));
    }
    let out = roles
        .iter()
        .filter_map(|r| snap.roles.get(r))
        .filter(|r| r.can_login)
        .map(|r| {
            let mut row = vec![b_text(&r.name), b_bool(snap.is_super(&r.name))];
            if d == Dialect::Cassandra {
                row.push(b_text(&authz::dc_permissions(db, &snap, &r.name).text()));
            }
            row
        })
        .collect();
    Ok(rows(cols, out))
}

// ---------------------------------------------------------------------------------------------
// Identities (Cassandra 5)
// ---------------------------------------------------------------------------------------------

async fn add_identity(
    db: &Arc<Database>,
    session: &SessionState,
    a: &ast::AddIdentity<'_>,
) -> Result<Response> {
    let (_g, snap) = store::lock(db).await?;
    let cx = Ctx::new(db, session);
    let (identity, role) = (a.identity.text.to_string(), a.role.text.to_string());
    role_perm(&cx, Permission::Create, &Resource::RolesRoot)?;
    if !cx.user_is_super() && snap.roles.get(&role).is_some_and(|r| r.is_superuser) {
        return Err(m::identity_superuser_only_add());
    }
    cx.ensure_not_anonymous()?;
    if !snap.exists(&role) {
        return Err(m::identity_role_missing(&role));
    }
    if snap.identities.contains_key(&identity) {
        if a.if_not_exists {
            return void();
        }
        return Err(m::identity_exists(&identity));
    }
    store::apply(
        db,
        vec![Write::new(
            "INSERT INTO system_auth.identity_to_role (identity, role) VALUES (?, ?)",
            vec![p_text(&identity), p_text(&role)],
        )],
        None,
    )
    .await?;
    store::reload(db).await?;
    void()
}

async fn drop_identity(
    db: &Arc<Database>,
    session: &SessionState,
    x: &ast::DropIdentity<'_>,
) -> Result<Response> {
    let (_g, snap) = store::lock(db).await?;
    let cx = Ctx::new(db, session);
    let identity = x.identity.text.to_string();
    match snap.identities.get(&identity) {
        None => role_perm(&cx, Permission::Drop, &Resource::RolesRoot)?,
        Some(owner) => {
            role_perm(&cx, Permission::Drop, &Resource::Role(owner.clone()))?;
            if !cx.user_is_super() && snap.is_super(owner) {
                return Err(m::identity_superuser_only_drop());
            }
        }
    }
    cx.ensure_not_anonymous()?;
    if !snap.identities.contains_key(&identity) {
        if x.if_exists {
            return void();
        }
        return Err(m::identity_missing(&identity));
    }
    store::apply(
        db,
        vec![Write::new(
            "DELETE FROM system_auth.identity_to_role WHERE identity = ?",
            vec![p_text(&identity)],
        )],
        None,
    )
    .await?;
    store::reload(db).await?;
    void()
}

// ---------------------------------------------------------------------------------------------
// Schema changes
// ---------------------------------------------------------------------------------------------

/// Grants the creator permissions on what a statement created (`AlterSchemaStatement.
/// grantPermissionsOnResource`, only with `CassandraAuthorizer` and a logged-in user) and
/// revokes every permission on what it dropped (`AuthSchemaChangeListener`).
pub async fn schema_changed(
    db: &Arc<Database>,
    session: &SessionState,
    before: &Schema,
    change: &SchemaChange,
) -> Result<()> {
    if !authz::enforcing(db) {
        return Ok(());
    }
    let d = db.dialect();
    let ks = change.keyspace.as_str();
    let name = change.name.as_deref().unwrap_or("");
    let function = |arg_types: &Option<Vec<String>>| -> Option<Resource> {
        let types: Vec<CqlType> = arg_types
            .iter()
            .flatten()
            .map(|t| nosql_types::parse_cql_type(t, ks, &|_, _| None).ok())
            .collect::<Option<_>>()?;
        Some(Resource::Function {
            keyspace: ks.to_string(),
            name: name.to_string(),
            marshal: types
                .iter()
                .map(|t| nosql_types::marshal_name(t, false, d))
                .collect(),
            cql: types.iter().map(nosql_types::cql_name).collect(),
        })
    };
    match change.change {
        ChangeType::Created => {
            let Some(user) = session.user.as_deref() else {
                return Ok(());
            };
            let created: Vec<Resource> = match change.target {
                ChangeTarget::Keyspace if before.keyspace(ks).is_none() => {
                    let mut v = vec![Resource::Keyspace(ks.to_string())];
                    if d == Dialect::Cassandra {
                        v.push(Resource::FunctionsIn(ks.to_string()));
                    }
                    v
                }
                ChangeTarget::Table => {
                    let existed = before
                        .keyspace(ks)
                        .is_some_and(|k| k.tables.contains_key(name));
                    let is_view = db
                        .schema()
                        .keyspace(ks)
                        .and_then(|k| k.tables.get(name).cloned())
                        .is_some_and(|t| t.view.is_some());
                    if existed || is_view {
                        Vec::new()
                    } else {
                        vec![Resource::Table(ks.to_string(), name.to_string())]
                    }
                }
                // ScyllaDB grants its creator nothing on a function or aggregate (probed).
                ChangeTarget::Function | ChangeTarget::Aggregate if d == Dialect::Cassandra => {
                    function(&change.arg_types).into_iter().collect()
                }
                _ => Vec::new(),
            };
            if created.is_empty() {
                return Ok(());
            }
            let (_g, mut snap) = store::lock(db).await?;
            if !snap.exists(user) {
                return Ok(());
            }
            for r in created {
                let perms = r.applicable(d).to_vec();
                store::apply(db, grant_writes(d, &snap, user, &r, &perms).0, None).await?;
                snap = store::reload(db).await?;
            }
            Ok(())
        }
        ChangeType::Dropped => {
            let mut gone: Vec<String> = Vec::new();
            match change.target {
                ChangeTarget::Keyspace => {
                    gone.push(Resource::Keyspace(ks.to_string()).name());
                    gone.push(Resource::AllTables(ks.to_string()).name());
                    gone.push(Resource::FunctionsIn(ks.to_string()).name());
                    if let Some(k) = before.keyspace(ks) {
                        for t in k.tables.keys() {
                            gone.push(Resource::Table(ks.to_string(), t.clone()).name());
                        }
                    }
                    let prefix = format!("functions/{ks}/");
                    let snap = db.auth.snapshot();
                    gone.extend(
                        snap.permissions
                            .values()
                            .flat_map(|m| m.keys())
                            .filter(|r| r.starts_with(&prefix))
                            .cloned(),
                    );
                }
                ChangeTarget::Table => {
                    gone.push(Resource::Table(ks.to_string(), name.to_string()).name())
                }
                ChangeTarget::Function | ChangeTarget::Aggregate => {
                    gone.extend(function(&change.arg_types).map(|r| r.name()))
                }
                _ => {}
            }
            if gone.is_empty() {
                return Ok(());
            }
            let (_g, snap) = store::lock(db).await?;
            let mut w = Vec::new();
            for r in gone.iter().collect::<BTreeSet<_>>() {
                w.extend(revoke_all_on(d, &snap, r));
            }
            store::apply(db, w, None).await?;
            store::reload(db).await?;
            Ok(())
        }
        ChangeType::Updated => Ok(()),
    }
}

/// `DcPermissions` re-export for the statement tests.
#[allow(dead_code)]
pub(crate) fn dc(db: &Database, role: &str) -> DcPermissions {
    authz::dc_permissions(db, &db.auth.snapshot(), role)
}
