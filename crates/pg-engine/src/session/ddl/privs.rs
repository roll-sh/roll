//! Roles, role membership, GRANT / REVOKE, default privileges and object ownership
//! (Postgres's user.c and aclchk.c).

use super::{Ddl, def_elems};
use crate::catalog::acl::{self, Acl, AclItem, AuthMember, DefaultAcl, ObjKind};
use crate::catalog::{BOOTSTRAP_SUPERUSER_OID, PG_DATABASE_OWNER_OID, Relation, Role};
use crate::error::{Error, Result, code};
use crate::session::warning;
use crate::sql::binder::ast::{const_str, inner, ival, names};
use crate::sql::binder::resolve_type_name;
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// An object a GRANT names.
struct Target {
    kind: ObjKind,
    oid: u32,
    /// Its name for messages.
    name: String,
    owner: u32,
    /// pg_class relkind for relations (tables, views, sequences).
    relkind: char,
    /// The word "permission denied for …" uses.
    what: &'static str,
}

/// A large object's oid as GRANT / ALTER / COMMENT name it (oidparse).
pub(super) fn node_oid(n: Option<&pb::Node>, fmt: &crate::types::text::FormatCtx) -> Result<u32> {
    match n.and_then(inner) {
        Some(NodeEnum::Integer(i)) => Ok(i.ival as u32),
        Some(NodeEnum::Float(f)) => {
            Ok(
                crate::types::text::parse_value(&f.fval, &DataType::Oid, fmt)?
                    .as_i64()
                    .unwrap_or(0) as u32,
            )
        }
        _ => Err(crate::error::internal(
            "unrecognized node type for a large object",
        )),
    }
}

/// A boolean DefElem argument (`SUPERUSER`, `INHERIT FALSE`, `WITH ADMIN OPTION`).
fn def_bool(d: &pb::DefElem) -> Option<bool> {
    match d.arg.as_deref().and_then(inner) {
        None => Some(true),
        Some(NodeEnum::Boolean(b)) => Some(b.boolval),
        Some(NodeEnum::Integer(i)) => Some(i.ival != 0),
        Some(_) => match d
            .arg
            .as_deref()
            .and_then(const_str)?
            .to_ascii_lowercase()
            .as_str()
        {
            "true" | "on" | "1" | "yes" => Some(true),
            "false" | "off" | "0" | "no" => Some(false),
            _ => None,
        },
    }
}

fn redundant(d: &pb::DefElem) -> Error {
    crate::sql::binder::ast::with_pos(
        Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
        d.location,
    )
}

impl Ddl<'_, '_> {
    /// current_user, as of the catalog being edited.
    pub(super) fn me(&self) -> u32 {
        self.ctx.sess.current_role(&self.cat)
    }

    pub(super) fn db_owner(&self) -> u32 {
        self.ctx.sess.db_owner(&self.cat)
    }

    pub(super) fn is_superuser(&self) -> bool {
        self.cat.is_superuser(self.me())
    }

    pub(super) fn role_name(&self, oid: u32) -> String {
        if oid == 0 {
            return "public".into();
        }
        self.cat
            .roles
            .get(&oid)
            .map_or_else(|| oid.to_string(), |r| r.name.clone())
    }

    /// The role a RoleSpec names (PUBLIC is 0).
    pub(super) fn role_oid(&self, r: &pb::RoleSpec) -> Result<u32> {
        match pb::RoleSpecType::try_from(r.roletype) {
            Ok(pb::RoleSpecType::RolespecCurrentUser | pb::RoleSpecType::RolespecCurrentRole) => {
                Ok(self.me())
            }
            Ok(pb::RoleSpecType::RolespecSessionUser) => Ok(self.ctx.sess.session_role(&self.cat)),
            Ok(pb::RoleSpecType::RolespecPublic) => Ok(0),
            _ => self.role_by_name(&r.rolename),
        }
    }

    fn role_by_name(&self, name: &str) -> Result<u32> {
        self.cat.role_by_name(name).map(|r| r.oid).ok_or_else(|| {
            Error::new(
                code::UNDEFINED_OBJECT,
                format!("role \"{name}\" does not exist"),
            )
        })
    }

    /// The roles of a list of RoleSpec nodes.
    fn role_list(&self, list: &[pb::Node]) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        for n in list {
            if let Some(NodeEnum::RoleSpec(r)) = inner(n) {
                out.push(self.role_oid(r)?);
            }
        }
        Ok(out)
    }

    /// Whether the current user holds a role attribute (superusers hold them all).
    fn has_attr(&self, f: impl Fn(&Role) -> bool) -> bool {
        self.cat
            .roles
            .get(&self.me())
            .is_some_and(|r| r.superuser || f(r))
    }

    /// roles_is_member_of's `admin_role`: the first of `member`'s roles (breadth first, through
    /// inheritable grants only when `privs`) holding ADMIN OPTION on `role` directly.
    fn admin_via(&self, member: u32, role: u32, privs: bool) -> Option<u32> {
        // By policy, a role cannot have WITH ADMIN OPTION on itself.
        if member == role {
            return None;
        }
        self.cat
            .roles_of(member, privs, self.db_owner())
            .into_iter()
            .find(|r| {
                self.cat
                    .members
                    .iter()
                    .any(|m| m.role == role && m.member == *r && m.admin)
            })
    }

    /// is_admin_of_role.
    pub(super) fn is_admin_of_role(&self, member: u32, role: u32) -> bool {
        self.cat.is_superuser(member) || self.admin_via(member, role, false).is_some()
    }

    /// select_best_admin (super-userness ignored).
    fn select_best_admin(&self, member: u32, role: u32) -> Option<u32> {
        self.admin_via(member, role, true)
    }

    /// check_role_membership_authorization: may the current user grant (revoke) `role`?
    fn check_membership_auth(&self, role: u32, grant: bool) -> Result<()> {
        let rname = self.role_name(role);
        if grant && role == PG_DATABASE_OWNER_OID {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("role \"{rname}\" cannot have explicit members"),
            ));
        }
        let verb = if grant { "grant" } else { "revoke" };
        let me = self.me();
        if self.cat.is_superuser(role) {
            if !self.cat.is_superuser(me) {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to {verb} role \"{rname}\""),
                )
                .detail(format!(
                    "Only roles with the SUPERUSER attribute may {verb} roles with the SUPERUSER attribute."
                )));
            }
        } else if !self.is_admin_of_role(me, role) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to {verb} role \"{rname}\""),
            )
            .detail(format!(
                "Only roles with the ADMIN option on role \"{rname}\" may {verb} this role."
            )));
        }
        Ok(())
    }

    // ── CREATE / ALTER / DROP ROLE ──

    fn role_options(&self, role: &mut Role, opts: &[pb::Node], create: bool) -> Result<RoleLists> {
        let mut seen: Vec<&str> = Vec::new();
        let mut lists = RoleLists::default();
        for d in def_elems(opts) {
            let key = d.defname.as_str();
            if seen.contains(&key) {
                return Err(redundant(d));
            }
            seen.push(key);
            let flag = || def_bool(d).unwrap_or(true);
            match key {
                "password" => {
                    // (Stored once the statement's checks pass: `set_password`.)
                    lists.password_raw = Some(d.arg.as_deref().and_then(const_str));
                    lists.password = true;
                }
                "superuser" => role.superuser = flag(),
                "canlogin" => role.login = flag(),
                "createdb" => role.create_db = flag(),
                "createrole" => role.create_role = flag(),
                "inherit" => role.inherit = flag(),
                "isreplication" => role.replication = flag(),
                "bypassrls" => role.bypass_rls = flag(),
                "connectionlimit" => {
                    let n = d.arg.as_deref().and_then(ival).unwrap_or(-1);
                    if n < -1 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("invalid connection limit: {n}"),
                        ));
                    }
                    role.conn_limit = n as i32;
                }
                "validUntil" => {
                    let v = d.arg.as_deref().and_then(const_str).unwrap_or_default();
                    let ts = crate::types::text::parse_value(
                        &v,
                        &DataType::Timestamptz(-1),
                        self.ctx.fmt(),
                    )?;
                    role.valid_until = match ts {
                        crate::types::Value::Timestamptz(t) => Some(t),
                        _ => None,
                    };
                }
                "addroleto" if create => {
                    lists.in_role = match d.arg.as_deref().and_then(inner) {
                        Some(NodeEnum::List(l)) => l.items.clone(),
                        _ => Vec::new(),
                    }
                }
                "rolemembers" if !create => {
                    lists.role = match d.arg.as_deref().and_then(inner) {
                        Some(NodeEnum::List(l)) => l.items.clone(),
                        _ => Vec::new(),
                    }
                }
                "rolemembers" if create => {
                    lists.role = match d.arg.as_deref().and_then(inner) {
                        Some(NodeEnum::List(l)) => l.items.clone(),
                        _ => Vec::new(),
                    }
                }
                "adminmembers" if create => {
                    lists.admin = match d.arg.as_deref().and_then(inner) {
                        Some(NodeEnum::List(l)) => l.items.clone(),
                        _ => Vec::new(),
                    }
                }
                "sysid" => self.notice("SYSID can no longer be specified"),
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("option \"{other}\" not recognized"),
                    ));
                }
            }
        }
        lists.seen = seen.iter().map(|s| s.to_string()).collect();
        Ok(lists)
    }

    /// A PASSWORD option, as CreateRole / AlterRole store it: encrypted per
    /// password_encryption, cleared when it is (a hash of) the empty string.
    fn set_password(&self, role: &mut Role, raw: Option<String>) -> Result<()> {
        let Some(p) = raw else {
            role.password = None;
            return Ok(());
        };
        // check_password_hook: passwordcheck, once loaded.
        let check = {
            let st = self.ctx.sess.settings.borrow();
            st.module_loaded("passwordcheck").then(|| st.int("passwordcheck.min_password_length"))
        };
        if let Some(min) = check {
            crate::auth::passwordcheck(&role.name, &p, min)?;
        }
        if p.is_empty() || crate::auth::is_empty_password(&role.name, &p) {
            self.notice("empty string is not a valid password, clearing password");
            role.password = None;
            return Ok(());
        }
        let (md5, iterations, warn) = {
            let st = self.ctx.sess.settings.borrow();
            (
                st.get_raw("password_encryption").as_deref() == Some("md5"),
                st.int("scram_iterations").clamp(1, i32::MAX as i64) as u32,
                st.bool("md5_password_warnings"),
            )
        };
        let enc = crate::auth::encrypt_password(md5, &role.name, &p, iterations);
        if enc.len() > 512 {
            return Err(Error::new(
                code::PROGRAM_LIMIT_EXCEEDED,
                "encrypted password is too long",
            )
            .detail("Encrypted passwords must be no longer than 512 bytes."));
        }
        if warn && crate::auth::password_type(&enc) == crate::auth::PasswordType::Md5 {
            self.ctx.notice(
                warning(code::WARNING_DEPRECATED_FEATURE, "setting an MD5-encrypted password")
                    .detail("MD5 password support is deprecated and will be removed in a future release of PostgreSQL.")
                    .hint("Refer to the PostgreSQL documentation for details about migrating to another password type."),
            );
        }
        role.password = Some(enc);
        Ok(())
    }

    fn node_roles(&self, n: &pb::Node) -> Result<Vec<u32>> {
        match inner(n) {
            Some(NodeEnum::List(l)) => self.role_list(&l.items),
            _ => Ok(Vec::new()),
        }
    }

    pub(super) fn create_role(&mut self, s: &pb::CreateRoleStmt) -> Result<String> {
        let me = self.me();
        let denied = |attr: &str| {
            Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied to create role",
            )
            .detail(format!(
                "Only roles with the {attr} attribute may create roles with the {attr} attribute."
            ))
        };
        let user = s.stmt_type == pb::RoleStmtType::RolestmtUser as i32;
        let mut role = Role {
            login: user,
            ..Role::new(0, s.role.clone())
        };
        if !self.has_attr(|r| r.create_role) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied to create role",
            )
            .detail("Only roles with the CREATEROLE attribute may create roles."));
        }
        let lists = self.role_options(&mut role, &s.options, true)?;
        if role.superuser && !self.is_superuser() {
            return Err(denied("SUPERUSER"));
        }
        if role.create_db && !self.has_attr(|r| r.create_db) {
            return Err(denied("CREATEDB"));
        }
        if role.replication && !self.has_attr(|r| r.replication) {
            return Err(denied("REPLICATION"));
        }
        if role.bypass_rls && !self.has_attr(|r| r.bypass_rls) {
            return Err(denied("BYPASSRLS"));
        }
        let name = s.role.clone();
        if matches!(name.as_str(), "public" | "none") {
            return Err(Error::new(
                code::RESERVED_NAME,
                format!("role name \"{name}\" is reserved"),
            ));
        }
        if name.starts_with("pg_") {
            return Err(Error::new(
                code::RESERVED_NAME,
                format!("role name \"{name}\" is reserved"),
            )
            .detail("Role names starting with \"pg_\" are reserved."));
        }
        if self.cat.role_by_name(&name).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("role \"{name}\" already exists"),
            ));
        }
        if let Some(raw) = lists.password_raw.clone() {
            self.set_password(&mut role, raw)?;
        }
        role.oid = self.cat.alloc_oid();
        let oid = role.oid;
        self.cat.roles.insert(oid, role);
        // IN ROLE: each role joined needs the right to grant it.
        for n in &lists.in_role {
            let Some(NodeEnum::RoleSpec(rs)) = inner(n) else {
                continue;
            };
            let r = self.role_oid(rs)?;
            self.check_membership_auth(r, true)?;
            let grantor = self.grantor_for(r, None, true)?;
            self.add_member(r, oid, grantor, None, None, None)?;
        }
        // A CREATEROLE user administers the roles it creates.
        if !self.cat.is_superuser(me) {
            let m = AuthMember {
                oid: self.cat.alloc_oid(),
                role: oid,
                member: me,
                grantor: BOOTSTRAP_SUPERUSER_OID,
                admin: true,
                inherit: false,
                set: false,
            };
            self.cat.members.push(m);
            let opts = self
                .ctx
                .sess
                .settings
                .borrow()
                .get_raw("createrole_self_grant")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !opts.trim().is_empty() {
                let has = |o: &str| opts.split(',').any(|x| x.trim() == o);
                self.add_member(oid, me, me, Some(false), Some(has("inherit")), Some(has("set")))?;
            }
        }
        // ROLE / ADMIN: members of the new role (no permission needed).
        for (list, admin) in [(&lists.role, false), (&lists.admin, true)] {
            let members = self.role_list(list)?;
            if members.is_empty() {
                continue;
            }
            let grantor = self.grantor_for(oid, None, true)?;
            self.member_checks(oid, &members)?;
            if admin {
                self.check_admin_circularity(oid, &members, grantor)?;
            }
            for m in members {
                self.add_member(oid, m, grantor, admin.then_some(true), None, None)?;
            }
        }
        self.save()?;
        Ok("CREATE ROLE".into())
    }

    /// May the current user change `role`'s attributes?
    fn check_alter_role(&self, role: &Role, verb: &str) -> Result<()> {
        if self.is_superuser() {
            return Ok(());
        }
        if role.superuser {
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to {verb} role"))
                .detail(format!("Only roles with the SUPERUSER attribute may {verb} roles with the SUPERUSER attribute.")));
        }
        if !self.has_attr(|r| r.create_role) || !self.is_admin_of_role(self.me(), role.oid) {
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to {verb} role"))
                .detail(format!("Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may {verb} this role.", role.name)));
        }
        Ok(())
    }

    pub(super) fn alter_role(&mut self, s: &pb::AlterRoleStmt) -> Result<String> {
        let r = s.role.as_ref().unwrap();
        let oid = self.role_oid(r)?;
        let Some(old) = self.cat.roles.get(&oid).cloned() else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("role \"{}\" does not exist", r.rolename),
            ));
        };
        let mut role = old.clone();
        let lists = self.role_options(&mut role, &s.options, false)?;
        let given = |k: &str| lists.seen.iter().any(|x| x == k);
        let denied = |detail: String| {
            Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied to alter role",
            )
            .detail(detail)
        };
        let attr = |a: &str| format!("Only roles with the {a} attribute may change the {a} attribute.");
        // AlterRole's checks, in its order.
        let su = self.is_superuser();
        if !su && old.superuser {
            return Err(denied(
                "Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute.".into(),
            ));
        }
        if !su && given("superuser") {
            return Err(denied(attr("SUPERUSER")));
        }
        let admin = self.is_admin_of_role(self.me(), oid);
        if !self.has_attr(|r| r.create_role) || !admin {
            if [
                "inherit",
                "createrole",
                "createdb",
                "canlogin",
                "connectionlimit",
                "validUntil",
                "isreplication",
                "bypassrls",
            ]
            .iter()
            .any(|k| given(k))
            {
                return Err(denied(format!(
                    "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may alter this role.",
                    old.name
                )));
            }
            if given("password") && oid != self.me() {
                return Err(denied("To change another role's password, the current user must have the CREATEROLE attribute and the ADMIN option on the role.".into()));
            }
        } else if !su {
            if given("createdb") && !self.has_attr(|r| r.create_db) {
                return Err(denied(attr("CREATEDB")));
            }
            if given("isreplication") && !self.has_attr(|r| r.replication) {
                return Err(denied(attr("REPLICATION")));
            }
            if given("bypassrls") && !self.has_attr(|r| r.bypass_rls) {
                return Err(denied(attr("BYPASSRLS")));
            }
        }
        // To add or drop members, you need ADMIN OPTION.
        if given("rolemembers") && !self.is_admin_of_role(self.me(), oid) {
            return Err(denied(format!(
                "Only roles with the ADMIN option on role \"{}\" may add or drop members.",
                old.name
            )));
        }
        if let Some(raw) = lists.password_raw.clone() {
            self.set_password(&mut role, raw)?;
        }
        if oid == BOOTSTRAP_SUPERUSER_OID && !role.superuser {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied to alter role",
            )
            .detail("The bootstrap superuser must have the SUPERUSER attribute."));
        }
        self.cat.roles.insert(oid, role);
        // ALTER GROUP ... ADD / DROP USER.
        if given("rolemembers") {
            let members = self.role_list(&lists.role)?;
            if s.action > 0 {
                let grantor = self.grantor_for(oid, None, true)?;
                self.member_checks(oid, &members)?;
                for m in members {
                    self.add_member(oid, m, grantor, None, None, None)?;
                }
            } else if s.action < 0 {
                let grantor = self.grantor_for(oid, None, false)?;
                self.del_role_mems(oid, &members, grantor, Rrg::Delete, false)?;
            }
        }
        self.save()?;
        Ok("ALTER ROLE".into())
    }

    /// ALTER ROLE … [IN DATABASE d] SET / RESET: stored for new sessions.
    pub(super) fn alter_role_set(&mut self, s: &pb::AlterRoleSetStmt) -> Result<String> {
        let role = match s.role.as_ref() {
            Some(r)
                if r.roletype != pb::RoleSpecType::RolespecPublic as i32
                    && !(r.roletype == pb::RoleSpecType::RolespecCstring as i32
                        && r.rolename.is_empty()) =>
            {
                let oid = self.role_oid(r)?;
                let old = self.cat.roles.get(&oid).cloned().unwrap();
                if oid != self.me() {
                    self.check_alter_role(&old, "alter")?;
                }
                oid
            }
            _ => {
                if !self.is_superuser() {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        "permission denied to alter role",
                    )
                    .detail(
                        "Only roles with the SUPERUSER attribute may alter settings globally.",
                    ));
                }
                0
            }
        };
        let db = if s.database.is_empty() {
            None
        } else {
            Some(s.database.clone())
        };
        let Some(set) = s.setstmt.as_ref() else {
            return Ok("ALTER ROLE".into());
        };
        self.store_role_setting(role, db, set)?;
        Ok("ALTER ROLE".into())
    }

    /// ALTER DATABASE ... SET / RESET (AlterDatabaseSet): the database's settings for every
    /// role, pg_db_role_setting's (database, 0) row; its owner (or a superuser) may.
    pub(super) fn alter_database_set(&mut self, s: &pb::AlterDatabaseSetStmt) -> Result<String> {
        let Some((_, _, owner)) = self
            .ctx
            .sess
            .cluster
            .databases()
            .into_iter()
            .find(|d| d.1 == s.dbname)
        else {
            return Err(Error::new(
                code::UNDEFINED_DATABASE,
                format!("database \"{}\" does not exist", s.dbname),
            ));
        };
        if !self.is_superuser() && !self.cat.has_privs_of_role(self.me(), owner, self.db_owner()) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of database {}", s.dbname),
            ));
        }
        if let Some(set) = s.setstmt.as_ref() {
            self.store_role_setting(0, Some(s.dbname.clone()), set)?;
        }
        Ok("ALTER DATABASE".into())
    }

    /// One SET / RESET / RESET ALL on a (role, database) entry of pg_db_role_setting.
    fn store_role_setting(
        &mut self,
        role: u32,
        db: Option<String>,
        set: &pb::VariableSetStmt,
    ) -> Result<()> {
        let entry = self
            .cat
            .role_settings
            .iter()
            .position(|e| e.role == role && e.database == db);
        let mut list = entry
            .map(|i| self.cat.role_settings[i].settings.clone())
            .unwrap_or_default();
        use pb::VariableSetKind as K;
        match K::try_from(set.kind) {
            Ok(K::VarSetValue) => {
                let value = super::super::utility::set_value(&set.name, &set.args)?;
                crate::session::settings::check_setting(&set.name, &value)?;
                // (validate_option_array_item finds the parameter creating placeholders: a
                // custom one now exists in this session, as LOAD's reservation sees.)
                self.ctx.sess.settings.borrow_mut().add_placeholder(&set.name);
                list.retain(|(n, _)| !n.eq_ignore_ascii_case(&set.name));
                list.push((set.name.clone(), value));
            }
            // (SET ... TO DEFAULT removes the entry, as RESET does.)
            Ok(K::VarReset | K::VarSetDefault) => {
                list.retain(|(n, _)| !n.eq_ignore_ascii_case(&set.name))
            }
            Ok(K::VarResetAll) => list.clear(),
            _ => {}
        }
        if let Some(i) = entry {
            self.cat.role_settings.remove(i);
        }
        if !list.is_empty() {
            self.cat.role_settings.push(crate::catalog::RoleSetting {
                role,
                database: db,
                settings: list,
            });
        }
        self.save()
    }

    pub(super) fn drop_role(&mut self, s: &pb::DropRoleStmt) -> Result<String> {
        if !self.has_attr(|r| r.create_role) {
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to drop role")
                .detail("Only roles with the CREATEROLE attribute and the ADMIN option on the target roles may drop roles."));
        }
        let mut dropping = Vec::new();
        // The roles are locked first (shared objects, AccessExclusiveLock), then looked at
        // as others left them.
        let mut locked = false;
        for r in &s.roles {
            if let Some(NodeEnum::RoleSpec(rs)) = inner(r)
                && let Some(role) = self.cat.role_by_name(&rs.rolename)
                && role.oid >= crate::catalog::FIRST_USER_OID
            {
                let oid = role.oid;
                self.lock_object(1260, oid, crate::txn::locks::LockMode::AccessExclusive)?;
                locked = true;
            }
        }
        if locked {
            self.refresh_catalog();
        }
        for r in &s.roles {
            let Some(NodeEnum::RoleSpec(rs)) = inner(r) else {
                continue;
            };
            let Some(role) = self.cat.role_by_name(&rs.rolename).cloned() else {
                if s.missing_ok {
                    self.notice(format!("role \"{}\" does not exist, skipping", rs.rolename));
                    continue;
                }
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("role \"{}\" does not exist", rs.rolename),
                ));
            };
            if role.oid == self.me() {
                return Err(Error::new(
                    code::OBJECT_IN_USE,
                    "current user cannot be dropped",
                ));
            }
            if role.oid == self.ctx.sess.session_role(&self.cat) {
                return Err(Error::new(
                    code::OBJECT_IN_USE,
                    "session user cannot be dropped",
                ));
            }
            if role.superuser && !self.is_superuser() {
                return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to drop role")
                    .detail("Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER attribute."));
            }
            if !self.is_admin_of_role(self.me(), role.oid) {
                return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to drop role")
                    .detail(format!("Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may drop this role.", role.name)));
            }
            // Its memberships go at once (later roles of the list see them gone).
            self.cat
                .members
                .retain(|m| m.role != role.oid && m.member != role.oid);
            if !dropping.iter().any(|d: &Role| d.oid == role.oid) {
                dropping.push(role);
            }
        }
        for role in &dropping {
            if role.oid == BOOTSTRAP_SUPERUSER_OID
                || crate::catalog::PREDEFINED_ROLES
                    .iter()
                    .any(|(o, _)| *o == role.oid)
            {
                return Err(Error::new(
                    code::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop role {} because it is required by the database system",
                        role.name
                    ),
                ));
            }
            let deps = self.role_dependents(role.oid)?;
            if !deps.is_empty() {
                return Err(Error::new(
                    code::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "role \"{}\" cannot be dropped because some objects depend on it",
                        role.name
                    ),
                )
                .detail(deps.join("\n")));
            }
            self.cat.roles.remove(&role.oid);
            self.cat
                .members
                .retain(|m| m.role != role.oid && m.member != role.oid);
            // Memberships it granted now show the bootstrap superuser as grantor.
            for m in &mut self.cat.members {
                if m.grantor == role.oid {
                    m.grantor = BOOTSTRAP_SUPERUSER_OID;
                }
            }
            self.cat.role_settings.retain(|e| e.role != role.oid);
        }
        self.save()?;
        Ok("DROP ROLE".into())
    }

    /// What keeps a role from being dropped: its pg_shdepend rows, described as
    /// checkSharedDependencies does (object oid order, at most 100).
    fn role_dependents(&self, role: u32) -> Result<Vec<String>> {
        use crate::catalog::shdepend;
        const MAX_REPORTED_DEPS: usize = 100;
        let los = self.large_objects()?;
        let los: Vec<(u32, u32, Option<&Acl>)> = los
            .iter()
            .map(|(o, m)| (*o, m.owner, m.acl.as_ref()))
            .collect();
        let dbs = self.ctx.sess.cluster.databases();
        let pairs: Vec<(u32, u32)> = dbs.iter().map(|d| (d.0, d.2)).collect();
        let mut deps: Vec<shdepend::ShDep> = self
            .cat
            .shdepends(&pairs, &los)
            .into_iter()
            .filter(|d| d.role == role)
            .collect();
        shdepend::sort(&mut deps);
        let mut out = Vec::new();
        let mut unreported = 0;
        for d in &deps {
            let desc = if d.classid == shdepend::DATABASE {
                match dbs.iter().find(|x| x.0 == d.objid) {
                    Some(x) => format!("database {}", crate::catalog::quote_ident(&x.1)),
                    None => continue,
                }
            } else {
                match crate::catalog::depend::describe(
                    &self.cat,
                    self.search_path(),
                    (d.classid, d.objid, d.objsubid),
                ) {
                    Some(x) => x,
                    None => continue,
                }
            };
            if out.len() >= MAX_REPORTED_DEPS {
                unreported += 1;
                continue;
            }
            out.push(match d.deptype {
                shdepend::OWNER => format!("owner of {desc}"),
                // (A role membership's description says it already.)
                shdepend::ACL if d.classid == shdepend::AUTH_MEMBERS => desc,
                shdepend::ACL => format!("privileges for {desc}"),
                _ => format!("target of {desc}"),
            });
        }
        // Objects in other databases are only counted.
        let mut unreported_dbs = 0;
        for (_, name, deps) in shdepend::remote(self.ctx.sess) {
            let n = deps.iter().filter(|d| d.role == role).count();
            if n == 0 {
                continue;
            }
            if out.len() >= MAX_REPORTED_DEPS {
                unreported_dbs += 1;
                continue;
            }
            out.push(format!(
                "{n} object{} in database {}",
                if n == 1 { "" } else { "s" },
                crate::catalog::quote_ident(&name)
            ));
        }
        if unreported > 0 {
            out.push(format!(
                "and {unreported} other object{} (see server log for list)",
                if unreported == 1 { "" } else { "s" }
            ));
        }
        if unreported_dbs > 0 {
            out.push(format!(
                "and objects in {unreported_dbs} other database{} (see server log for list)",
                if unreported_dbs == 1 { "" } else { "s" }
            ));
        }
        Ok(out)
    }

    /// Text search configurations and dictionaries with their owners (the bootstrap
    /// superuser's aren't recorded).
    fn owned_ts(&self) -> Vec<(crate::catalog::tsearch::TsKind, u32, u32)> {
        use crate::catalog::tsearch::TsKind;
        let mut out = Vec::new();
        for k in [TsKind::Config, TsKind::Dict] {
            for (oid, _, _) in self.cat.ts_objects(k) {
                let owner = self.cat.ts_owner(k, oid);
                if owner != crate::catalog::BOOTSTRAP_SUPERUSER_OID {
                    out.push((k, oid, owner));
                }
            }
        }
        out
    }

    fn ts_description(&self, kind: crate::catalog::tsearch::TsKind, oid: u32) -> String {
        crate::catalog::depend::describe(
            &self.cat,
            self.search_path(),
            (crate::catalog::depend::ts_class(kind), oid, 0),
        )
        .unwrap_or_default()
    }

    /// Every object with an owner: (kind, oid, owner).
    fn owned_objects(&self) -> Vec<(ObjKind, u32, u32)> {
        let c = &self.cat;
        let mut out = Vec::new();
        out.extend(
            c.schemas
                .values()
                .filter(|s| {
                    !s.name.starts_with("pg_temp") && s.oid >= crate::catalog::FIRST_USER_OID
                })
                .map(|s| (ObjKind::Schema, s.oid, s.owner)),
        );
        out.extend(c.tables.values().map(|t| (ObjKind::Table, t.oid, t.owner)));
        out.extend(c.views.values().map(|v| (ObjKind::Table, v.oid, v.owner)));
        out.extend(
            c.sequences
                .values()
                .filter(|q| q.owned_by.is_none())
                .map(|q| (ObjKind::Sequence, q.oid, q.owner)),
        );
        out.extend(
            c.functions
                .values()
                .map(|f| (ObjKind::Function, f.oid, f.owner)),
        );
        out.extend(c.enums.values().map(|e| (ObjKind::Type, e.oid, e.owner)));
        out.extend(
            c.composites
                .values()
                .map(|e| (ObjKind::Type, e.oid, e.owner)),
        );
        out.extend(c.domains.values().map(|e| (ObjKind::Type, e.oid, e.owner)));
        out.extend(c.ranges.values().map(|e| (ObjKind::Type, e.oid, e.owner)));
        out.sort_by_key(|o| o.1);
        out
    }

    /// "table t", "column a of table t", "schema s" (getObjectDescription).
    fn describe(&self, kind: ObjKind, oid: u32, sub: i32) -> String {
        let c = &self.cat;
        match kind {
            ObjKind::Table | ObjKind::Sequence => {
                let (word, schema, name) = if let Some(t) = c.tables.get(&oid) {
                    ("table", t.schema, t.name.clone())
                } else if let Some(v) = c.views.get(&oid) {
                    (
                        if v.materialized {
                            "materialized view"
                        } else {
                            "view"
                        },
                        v.schema,
                        v.name.clone(),
                    )
                } else if let Some(q) = c.sequences.get(&oid) {
                    ("sequence", q.schema, q.name.clone())
                } else {
                    return format!("relation {oid}");
                };
                let rel = self.qualified(schema, &name);
                if sub > 0
                    && let Some(t) = c.tables.get(&oid)
                    && let Some(col) = t.columns.get(sub as usize - 1)
                {
                    return format!("column {} of {word} {rel}", col.name);
                }
                format!("{word} {rel}")
            }
            ObjKind::Schema => format!("schema {}", c.schema_name(oid)),
            ObjKind::Function => match c.functions.get(&oid) {
                Some(f) => format!(
                    "{} {}({})",
                    if f.procedure { "procedure" } else { "function" },
                    {
                        // format_procedure: qualified unless the path finds this function first.
                        let first = c.effective_path(self.search_path()).into_iter().find(|sc| {
                            c.functions
                                .values()
                                .any(|g| g.schema == *sc && g.name == f.name && g.same_params(f))
                        });
                        if first == Some(f.schema) {
                            crate::catalog::quote_ident(&f.name)
                        } else {
                            format!(
                                "{}.{}",
                                crate::catalog::quote_ident(c.schema_name(f.schema)),
                                crate::catalog::quote_ident(&f.name)
                            )
                        }
                    },
                    c.param_type_names(f, self.search_path()).join(",")
                ),
                None => format!("function {oid}"),
            },
            ObjKind::Type => {
                let name = (c.user_type_name(oid).map(|(s, n, _)| (s, n)))
                    .or_else(|| c.domains.get(&oid).map(|d| (d.schema, d.name.as_str())))
                    .map(|(s, n)| self.qualified(s, n))
                    .unwrap_or_else(|| oid.to_string());
                format!("type {name}")
            }
            ObjKind::Database => format!("database {}", self.ctx.sess.database),
            k => format!("{} {oid}", k.word()),
        }
    }

    // ── GRANT / REVOKE role ──

    /// The grantor a GRANT / REVOKE of `role` acts as (check_role_grantor).
    fn grantor_for(&self, role: u32, granted_by: Option<u32>, grant: bool) -> Result<u32> {
        let me = self.me();
        let Some(g) = granted_by else {
            if self.cat.is_superuser(me) {
                return Ok(BOOTSTRAP_SUPERUSER_OID);
            }
            return self
                .select_best_admin(me, role)
                .ok_or_else(|| crate::error::internal("no possible grantors"));
        };
        let gname = self.role_name(g);
        let db_owner = self.db_owner();
        if grant {
            if !self.cat.has_privs_of_role(me, g, db_owner) {
                return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to grant privileges as role \"{gname}\""))
                    .detail(format!("Only roles with privileges of role \"{gname}\" may grant privileges as this role.")));
            }
            if g != BOOTSTRAP_SUPERUSER_OID && self.select_best_admin(g, role) != Some(g) {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to grant privileges as role \"{gname}\""),
                )
                .detail(format!(
                    "The grantor must have the ADMIN option on role \"{}\".",
                    self.role_name(role)
                )));
            }
        } else if !self.cat.has_privs_of_role(me, g, db_owner) {
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to revoke privileges granted by role \"{gname}\""))
                .detail(format!("Only roles with privileges of role \"{gname}\" may revoke privileges granted by this role.")));
        }
        Ok(g)
    }

    /// AddRoleMems for one (role, member): new membership or changed options.
    fn add_member(
        &mut self,
        role: u32,
        member: u32,
        grantor: u32,
        admin: Option<bool>,
        inherit: Option<bool>,
        set: Option<bool>,
    ) -> Result<()> {
        let (rname, mname) = (self.role_name(role), self.role_name(member));
        if member == PG_DATABASE_OWNER_OID {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                format!("role \"{mname}\" cannot be a member of any role"),
            ));
        }
        // No cycles: the granted role must not already (transitively) contain the member.
        if role == member || self.cat.roles_of(role, false, 0).contains(&member) {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                format!("role \"{rname}\" is a member of role \"{mname}\""),
            ));
        }
        if let Some(m) = self
            .cat
            .members
            .iter_mut()
            .find(|m| m.role == role && m.member == member && m.grantor == grantor)
        {
            let changed = admin.is_some_and(|v| v != m.admin)
                || inherit.is_some_and(|v| v != m.inherit)
                || set.is_some_and(|v| v != m.set);
            if !changed {
                let g = grantor;
                let gname = self.role_name(g);
                self.notice(format!("role \"{mname}\" has already been granted membership in role \"{rname}\" by role \"{gname}\""));
                return Ok(());
            }
            let m = self
                .cat
                .members
                .iter_mut()
                .find(|m| m.role == role && m.member == member && m.grantor == grantor)
                .unwrap();
            if let Some(v) = admin {
                m.admin = v;
            }
            if let Some(v) = inherit {
                m.inherit = v;
            }
            if let Some(v) = set {
                m.set = v;
            }
            return Ok(());
        }
        let inherit =
            inherit.unwrap_or_else(|| self.cat.roles.get(&member).is_none_or(|r| r.inherit));
        let oid = self.cat.alloc_oid();
        self.cat.members.push(AuthMember {
            oid,
            role,
            member,
            grantor,
            admin: admin.unwrap_or(false),
            inherit,
            set: set.unwrap_or(true),
        });
        Ok(())
    }

    pub(super) fn grant_role(&mut self, s: &pb::GrantRoleStmt) -> Result<String> {
        let tag = if s.is_grant {
            "GRANT ROLE"
        } else {
            "REVOKE ROLE"
        };
        let (mut admin, mut inherit, mut set) = (None, None, None);
        for d in def_elems(&s.opt) {
            let v = def_bool(d).ok_or_else(|| {
                Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "unrecognized value for role option \"{}\": \"{}\"",
                        d.defname,
                        d.arg.as_deref().and_then(const_str).unwrap_or_default()
                    ),
                )
            })?;
            match d.defname.as_str() {
                "admin" => admin = Some(v),
                "inherit" => inherit = Some(v),
                "set" => set = Some(v),
                o => {
                    return Err(crate::sql::binder::ast::with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            format!("unrecognized role option \"{o}\""),
                        ),
                        d.location,
                    ));
                }
            }
        }
        let granted_by = match &s.grantor {
            Some(g) => Some(self.role_oid(g)?),
            None => None,
        };
        let cascade = s.behavior == pb::DropBehavior::DropCascade as i32;
        let mut roles = Vec::new();
        for p in &s.granted_roles {
            if let Some(NodeEnum::AccessPriv(a)) = inner(p) {
                roles.push(self.role_by_name(&a.priv_name)?);
            }
        }
        let members = self.role_list(&s.grantee_roles)?;
        for &role in &roles {
            self.check_membership_auth(role, s.is_grant)?;
            let grantor = self.grantor_for(role, granted_by, s.is_grant)?;
            let members: Vec<u32> = members.iter().copied().filter(|m| *m != 0).collect();
            if s.is_grant {
                self.member_checks(role, &members)?;
                if admin == Some(true) {
                    self.check_admin_circularity(role, &members, grantor)?;
                }
                for &member in &members {
                    self.add_member(role, member, grantor, admin, inherit, set)?;
                }
            } else {
                let which = if admin.is_some() {
                    Rrg::RemoveAdmin
                } else if inherit.is_some() {
                    Rrg::RemoveInherit
                } else if set.is_some() {
                    Rrg::RemoveSet
                } else {
                    Rrg::Delete
                };
                self.del_role_mems(role, &members, grantor, which, cascade)?;
            }
        }
        self.save()?;
        Ok(tag.into())
    }

    /// The grants of `role` (its pg_auth_members rows in (member, grantor) order).
    fn memlist(&self, role: u32) -> Vec<AuthMember> {
        let mut l: Vec<AuthMember> = self
            .cat
            .members
            .iter()
            .filter(|m| m.role == role)
            .cloned()
            .collect();
        l.sort_by_key(|m| (m.member, m.grantor));
        l
    }

    /// AddRoleMems' preliminary checks of the members.
    fn member_checks(&self, role: u32, members: &[u32]) -> Result<()> {
        for &member in members {
            let (rname, mname) = (self.role_name(role), self.role_name(member));
            if member == PG_DATABASE_OWNER_OID {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("role \"{mname}\" cannot be a member of any role"),
                ));
            }
            if role == member || self.cat.roles_of(role, false, 0).contains(&member) {
                return Err(Error::new(
                    code::INVALID_GRANT_OPERATION,
                    format!("role \"{rname}\" is a member of role \"{mname}\""),
                ));
            }
        }
        Ok(())
    }

    /// AddRoleMems: granting ADMIN OPTION back to whoever the grantor's own ADMIN OPTION
    /// rests on would make the grants circular.
    fn check_admin_circularity(&self, role: u32, members: &[u32], grantor: u32) -> Result<()> {
        if grantor == BOOTSTRAP_SUPERUSER_OID {
            return Ok(());
        }
        let list = self.memlist(role);
        let mut actions = vec![Rrg::Noop; list.len()];
        let back = || {
            Error::new(
                code::INVALID_GRANT_OPERATION,
                "ADMIN option cannot be granted back to your own grantor",
            )
        };
        for &member in members {
            if member == BOOTSTRAP_SUPERUSER_OID {
                return Err(back());
            }
            for i in 0..list.len() {
                if list[i].member == member {
                    plan_recursive_revoke(&list, &mut actions, i, false, true)?;
                }
            }
        }
        if !(0..list.len())
            .any(|i| actions[i] == Rrg::Noop && list[i].member == grantor && list[i].admin)
        {
            return Err(back());
        }
        Ok(())
    }

    /// DelRoleMems: revoke `role` (or one of its options, `which`) from `members` as granted by
    /// `grantor`, with the grants resting on it (CASCADE) or refusing if there are any.
    fn del_role_mems(
        &mut self,
        role: u32,
        members: &[u32],
        grantor: u32,
        which: Rrg,
        cascade: bool,
    ) -> Result<()> {
        let list = self.memlist(role);
        let mut actions = vec![Rrg::Noop; list.len()];
        for &member in members {
            let Some(i) = list
                .iter()
                .position(|m| m.member == member && m.grantor == grantor)
            else {
                let (rname, mname, gname) = (
                    self.role_name(role),
                    self.role_name(member),
                    self.role_name(grantor),
                );
                self.ctx.notice(warning(code::WARNING, format!("role \"{mname}\" has not been granted membership in role \"{rname}\" by role \"{gname}\"")));
                continue;
            };
            match which {
                Rrg::RemoveInherit | Rrg::RemoveSet => actions[i] = which,
                _ => plan_recursive_revoke(&list, &mut actions, i, which == Rrg::RemoveAdmin, cascade)?,
            }
        }
        for (m, a) in list.iter().zip(&actions) {
            let Some(row) = self.cat.members.iter().position(|x| x.oid == m.oid) else {
                continue;
            };
            match a {
                Rrg::Noop => {}
                Rrg::Delete => {
                    self.cat.members.remove(row);
                }
                Rrg::RemoveAdmin => self.cat.members[row].admin = false,
                Rrg::RemoveInherit => self.cat.members[row].inherit = false,
                Rrg::RemoveSet => self.cat.members[row].set = false,
            }
        }
        Ok(())
    }

    // ── GRANT / REVOKE on objects ──

    pub(super) fn grant(&mut self, s: &pb::GrantStmt) -> Result<String> {
        let tag = if s.is_grant { "GRANT" } else { "REVOKE" };
        use pb::ObjectType as O;
        let objtype = O::try_from(s.objtype).unwrap_or(O::ObjectTable);
        let (all_privs, errword) = grant_kind(objtype)?;
        let mut privs = 0;
        let mut col_privs: Vec<(String, u32)> = Vec::new();
        for p in &s.privileges {
            let Some(NodeEnum::AccessPriv(a)) = inner(p) else {
                continue;
            };
            // ALL (cols): every column privilege.
            let bit = if a.priv_name.is_empty() && !a.cols.is_empty() {
                Some(acl::SELECT | acl::INSERT | acl::UPDATE | acl::REFERENCES)
            } else {
                acl::privilege_by_name(&a.priv_name)
            };
            let bit = bit.ok_or_else(|| {
                Error::new(
                    code::SYNTAX_ERROR,
                    format!("unrecognized privilege type \"{}\"", a.priv_name),
                )
            })?;
            if !a.cols.is_empty() {
                if objtype != O::ObjectTable {
                    return Err(Error::new(
                        code::INVALID_GRANT_OPERATION,
                        "column privileges are only valid for relations",
                    ));
                }
                for c in names(&a.cols) {
                    col_privs.push((c, bit));
                }
                continue;
            }
            if bit & !all_privs != 0 {
                return Err(Error::new(
                    code::INVALID_GRANT_OPERATION,
                    format!(
                        "invalid privilege type {} for {errword}",
                        acl::privilege_name(bit)
                    ),
                ));
            }
            privs |= bit;
        }
        let all = s.privileges.is_empty();
        let grantees = self.role_list(&s.grantees)?;
        if let Some(g) = &s.grantor {
            let g = self.role_oid(g)?;
            if g != self.me() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "grantor must be current user",
                ));
            }
        }
        if s.is_grant && s.grant_option && grantees.contains(&0) {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                "grant options can only be granted to roles",
            ));
        }
        let cascade = s.behavior == pb::DropBehavior::DropCascade as i32;
        let mut targets = self.grant_targets(s, objtype)?;
        // ExecGrant_Relation / ExecGrant_Database update the object's pg_class /
        // pg_database row (SearchSysCacheLocked1, then heap_update): they wait for another
        // transaction updating or locking it, then work over what it committed.
        let mut locked = false;
        for t in &targets {
            let class = match t.kind {
                ObjKind::Table => 1259,
                ObjKind::Database => 1262,
                _ => continue,
            };
            self.ctx.txn.borrow_mut().lock_catalog_row(
                class,
                t.oid,
                crate::txn::locks::RowLockMode::NoKeyUpdate,
                true,
            )?;
            locked = true;
        }
        if locked && self.reload_after_wait() {
            targets = self.grant_targets(s, objtype)?;
        }
        // A parameter gets its pg_parameter_acl row when first granted on (revoking from
        // one without has nothing to do).
        if objtype == O::ObjectParameterAcl {
            targets.retain(|t| t.oid != 0 || s.is_grant);
            for t in &mut targets {
                if t.oid == 0 {
                    t.oid = match self.cat.parameters.iter().find(|p| p.1 == t.name) {
                        Some(p) => p.0,
                        None => {
                            let oid = self.cat.alloc_oid();
                            self.cat.parameters.push((oid, t.name.clone()));
                            oid
                        }
                    };
                }
            }
        }
        let params: Vec<u32> = targets
            .iter()
            .filter(|t| t.kind == ObjKind::Parameter)
            .map(|t| t.oid)
            .collect();
        for t in targets {
            let allowed = t.kind.all();
            let mut want = if all { allowed } else { privs };
            if t.relkind == 'S' && objtype == O::ObjectTable && !all && want & !allowed != 0 {
                self.ctx.notice(warning(
                    code::WARNING,
                    format!(
                        "sequence \"{}\" only supports USAGE, SELECT, and UPDATE privileges",
                        t.name
                    ),
                ));
                want &= allowed;
            } else if t.relkind != 'S' && t.kind == ObjKind::Table && want & !allowed != 0 {
                let bad = acl::NAMES
                    .iter()
                    .find(|(b, _)| want & !allowed & b != 0)
                    .map_or("?", |n| n.1);
                return Err(Error::new(
                    code::INVALID_GRANT_OPERATION,
                    format!("invalid privilege type {bad} for table"),
                ));
            }
            if want != 0 || col_privs.is_empty() {
                self.grant_one(
                    &t,
                    0,
                    None,
                    want,
                    all,
                    s.is_grant,
                    s.grant_option,
                    &grantees,
                    cascade,
                )?;
            }
            if !s.is_grant && t.kind == ObjKind::Table && want != 0 {
                // Revoking table privileges revokes them from every column too.
                let cols: Vec<i32> = self
                    .cat
                    .acls
                    .iter()
                    .filter(|a| a.class == 1259 && a.oid == t.oid && a.sub != 0)
                    .map(|a| a.sub)
                    .collect();
                for sub in cols {
                    self.grant_one(
                        &t,
                        sub,
                        None,
                        want & acl::COLUMN_PRIVS,
                        true,
                        false,
                        s.grant_option,
                        &grantees,
                        cascade,
                    )?;
                }
            }
            for (col, bit) in &col_privs {
                let Some(tbl) = self.cat.tables.get(&t.oid).cloned() else {
                    // (A system catalog's columns.)
                    if let Some(st) = crate::catalog::system::SysTable::by_oid(t.oid) {
                        let fields = match st.row_type() {
                            Some(crate::types::DataType::Row(r)) => r.fields.clone(),
                            _ => Vec::new(),
                        };
                        // (Its system columns too: ctid is -1, tableoid -6.)
                        let sys = crate::exec::scan::SYS_COLS
                            .iter()
                            .position(|(n, _)| n == col)
                            .map(|k| -(k as i32) - 1);
                        let pos = fields.iter().position(|f| &f.0 == col).map(|i| i as i32 + 1);
                        let Some(attnum) = pos.or(sys) else {
                            return Err(Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "column \"{col}\" of relation \"{}\" does not exist",
                                    t.name
                                ),
                            ));
                        };
                        self.grant_one(
                            &t,
                            attnum,
                            Some(col),
                            *bit,
                            false,
                            s.is_grant,
                            s.grant_option,
                            &grantees,
                            cascade,
                        )?;
                        continue;
                    }
                    if let Some(v) = self.cat.views.get(&t.oid).cloned() {
                        let Some(i) = v.columns.iter().position(|c| &c.0 == col) else {
                            return Err(Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "column \"{col}\" of relation \"{}\" does not exist",
                                    t.name
                                ),
                            ));
                        };
                        self.grant_one(
                            &t,
                            i as i32 + 1,
                            Some(col),
                            *bit,
                            false,
                            s.is_grant,
                            s.grant_option,
                            &grantees,
                            cascade,
                        )?;
                    }
                    continue;
                };
                // (System columns have privileges of their own: attnum -1 … -6.)
                let attnum = match tbl.column_index(col) {
                    Some(i) => i as i32 + 1,
                    None => match crate::exec::scan::SYS_COLS.iter().position(|(n, _)| n == col) {
                        Some(i) => -(i as i32 + 1),
                        None => {
                            return Err(Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "column \"{col}\" of relation \"{}\" does not exist",
                                    t.name
                                ),
                            ));
                        }
                    },
                };
                if bit & !acl::COLUMN_PRIVS != 0 {
                    return Err(Error::new(
                        code::INVALID_GRANT_OPERATION,
                        format!(
                            "invalid privilege type {} for column",
                            acl::privilege_name(*bit)
                        ),
                    ));
                }
                self.grant_one(
                    &t,
                    attnum,
                    Some(col),
                    *bit,
                    false,
                    s.is_grant,
                    s.grant_option,
                    &grantees,
                    cascade,
                )?;
            }
        }
        // A parameter whose ACL is back to the default loses its pg_parameter_acl row.
        for oid in params {
            let default = acl::initial_acl(ObjKind::Parameter, oid, BOOTSTRAP_SUPERUSER_OID);
            if self
                .cat
                .acl(ObjKind::Parameter, oid, 0)
                .is_none_or(|a| a.is_empty() || same_acl(a, &default))
            {
                self.cat.set_acl(ObjKind::Parameter, oid, 0, None);
                self.cat.parameters.retain(|p| p.0 != oid);
            }
        }
        self.save()?;
        Ok(tag.into())
    }

    /// restrict_and_check_grant + merge_acl_with_grant for one object (or column `sub`).
    #[allow(clippy::too_many_arguments)]
    fn grant_one(
        &mut self,
        t: &Target,
        sub: i32,
        col: Option<&str>,
        privs: u32,
        all: bool,
        is_grant: bool,
        grant_option: bool,
        grantees: &[u32],
        cascade: bool,
    ) -> Result<()> {
        let me = self.me();
        let db_owner = self.db_owner();
        let stored = match t.kind {
            ObjKind::LargeObject => self.lo_meta(t.oid)?.acl,
            _ => self.cat.acl(t.kind, t.oid, sub).cloned(),
        };
        let old = match &stored {
            Some(a) => a.clone(),
            None if sub != 0 => Vec::new(),
            None => acl::initial_acl(t.kind, t.oid, t.owner),
        };
        // Grant options held count table-level ones for columns too.
        let mut held_acl = old.clone();
        if sub != 0 {
            held_acl.extend(self.cat.effective_acl(t.kind, t.oid, t.owner));
        }
        let (grantor, avail) = self
            .cat
            .best_grantor(me, privs, &held_acl, t.owner, db_owner);
        if avail == 0 && !self.cat.is_superuser(me) {
            let (p, g) = self.cat.aclmask(&held_acl, grantor, t.owner, db_owner);
            let whole = if sub != 0 {
                acl::COLUMN_PRIVS
            } else {
                t.kind.all()
            };
            if (p | g) & whole == 0 {
                return Err(match col {
                    Some(c) => Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for column {c} of relation {}", t.name),
                    ),
                    None => Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for {} {}", t.what, t.name),
                    ),
                });
            }
        }
        let this = privs & avail;
        let target = match col {
            Some(c) => format!("column \"{c}\" of relation \"{}\"", t.name),
            None => format!("\"{}\"", t.name),
        };
        if is_grant {
            if this == 0 {
                self.ctx.notice(warning(
                    code::WARNING_PRIVILEGE_NOT_GRANTED,
                    format!("no privileges were granted for {target}"),
                ));
            } else if !all && this != privs {
                self.ctx.notice(warning(
                    code::WARNING_PRIVILEGE_NOT_GRANTED,
                    format!("not all privileges were granted for {target}"),
                ));
            }
        } else if this == 0 {
            self.ctx.notice(warning(
                code::WARNING_PRIVILEGE_NOT_REVOKED,
                format!("no privileges could be revoked for {target}"),
            ));
        } else if !all && this != privs {
            self.ctx.notice(warning(
                code::WARNING_PRIVILEGE_NOT_REVOKED,
                format!("not all privileges could be revoked for {target}"),
            ));
        }
        let mut new = old.clone();
        for &g in grantees {
            let (p, o) = if is_grant {
                (this, if grant_option { this } else { 0 })
            } else if grant_option {
                (0, this)
            } else {
                (this, this)
            };
            acl::acl_update(&mut new, g, grantor, p, o, is_grant, t.owner, cascade)?;
        }
        if stored.is_some() || new != old {
            if t.kind == ObjKind::LargeObject {
                let mut m = self.lo_meta(t.oid)?;
                m.acl = Some(new);
                self.lo_put(t.oid, &m)?;
            } else {
                self.cat.set_acl(
                    t.kind,
                    t.oid,
                    sub,
                    if sub != 0 && new.is_empty() {
                        None
                    } else {
                        Some(new)
                    },
                );
            }
        }
        Ok(())
    }

    /// A large object's metadata, as the catalog snapshot sees it.
    pub(super) fn lo_meta(&self, oid: u32) -> Result<crate::largeobj::LoMeta> {
        crate::largeobj::existing(&self.ctx.txn.borrow(), oid, crate::largeobj::Snap::Latest)
    }

    pub(super) fn lo_put(&mut self, oid: u32, m: &crate::largeobj::LoMeta) -> Result<()> {
        crate::largeobj::put_meta(&mut self.ctx.txn.borrow_mut(), oid, m)
    }

    /// Every large object the transaction sees.
    fn large_objects(&self) -> Result<Vec<(u32, crate::largeobj::LoMeta)>> {
        crate::largeobj::all(&self.ctx.txn.borrow())
    }

    /// The objects a GRANT names.
    fn grant_targets(&self, s: &pb::GrantStmt, objtype: pb::ObjectType) -> Result<Vec<Target>> {
        use pb::ObjectType as O;
        let mut out = Vec::new();
        let in_schema = s.targtype == pb::GrantTargetType::AclTargetAllInSchema as i32;
        if in_schema {
            for n in &s.objects {
                let Some(name) = const_str(n) else { continue };
                let schema = self.cat.schema_oid(&name)?;
                match objtype {
                    O::ObjectTable => {
                        for t in self.cat.tables.values().filter(|t| t.schema == schema) {
                            out.push(self.rel_target(&Relation::Table(t.clone())));
                        }
                        for v in self.cat.views.values().filter(|v| v.schema == schema) {
                            out.push(self.rel_target(&Relation::View(v.clone())));
                        }
                    }
                    O::ObjectSequence => {
                        for q in self.cat.sequences.values().filter(|q| q.schema == schema) {
                            out.push(self.rel_target(&Relation::Sequence(q.clone())));
                        }
                    }
                    _ => {
                        for f in self.cat.functions.values().filter(|f| f.schema == schema) {
                            let want = match objtype {
                                O::ObjectProcedure => f.procedure,
                                O::ObjectFunction => !f.procedure,
                                _ => true,
                            };
                            if want {
                                out.push(self.func_target(f));
                            }
                        }
                    }
                }
            }
            out.sort_by_key(|t| t.oid);
            return Ok(out);
        }
        for n in &s.objects {
            match (objtype, inner(n)) {
                (O::ObjectTable | O::ObjectSequence, Some(NodeEnum::RangeVar(rv))) => {
                    // A system catalog takes grants too (its ACL is then stored).
                    let Some(rel) = self.lookup(rv)? else {
                        if objtype == O::ObjectTable
                            && let Some(st) = crate::catalog::system::SysTable::lookup(
                                Self::rv_schema(rv),
                                &rv.relname,
                                self.search_path(),
                            )
                        {
                            out.push(Target {
                                kind: ObjKind::Table,
                                oid: st.oid(),
                                name: st.name().to_string(),
                                owner: BOOTSTRAP_SUPERUSER_OID,
                                relkind: if st.is_view() { 'v' } else { 'r' },
                                what: if st.is_view() { "view" } else { "table" },
                            });
                            continue;
                        }
                        return Err(crate::catalog::no_such_relation(
                            Self::rv_schema(rv),
                            &rv.relname,
                        ));
                    };
                    if objtype == O::ObjectSequence && !matches!(rel, Relation::Sequence(_)) {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("\"{}\" is not a sequence", rv.relname),
                        ));
                    }
                    if let Relation::Index(_) = rel {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("\"{}\" is an index", rv.relname),
                        ));
                    }
                    out.push(self.rel_target(&rel));
                }
                (O::ObjectSchema, _) => {
                    let name = const_str(n).unwrap_or_default();
                    let sc = self.cat.schema_by_name(&name).ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_SCHEMA,
                            format!("schema \"{name}\" does not exist"),
                        )
                    })?;
                    out.push(Target {
                        kind: ObjKind::Schema,
                        oid: sc.oid,
                        name: sc.name.clone(),
                        owner: sc.owner,
                        relkind: ' ',
                        what: "schema",
                    });
                }
                (O::ObjectDatabase, _) => {
                    let name = const_str(n).unwrap_or_default();
                    let Some((oid, _, _)) = self
                        .ctx
                        .sess
                        .cluster
                        .databases()
                        .into_iter()
                        .find(|d| d.1 == name)
                    else {
                        return Err(Error::new(
                            code::UNDEFINED_DATABASE,
                            format!("database \"{name}\" does not exist"),
                        ));
                    };
                    let owner = self
                        .ctx
                        .sess
                        .cluster
                        .database_owner(&name)
                        .and_then(|o| self.cat.role_by_name(&o).map(|r| r.oid))
                        .unwrap_or(BOOTSTRAP_SUPERUSER_OID);
                    out.push(Target {
                        kind: ObjKind::Database,
                        oid,
                        name,
                        owner,
                        relkind: ' ',
                        what: "database",
                    });
                }
                (
                    O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine,
                    Some(NodeEnum::ObjectWithArgs(owa)),
                ) => {
                    let word = match objtype {
                        O::ObjectProcedure => "procedure",
                        O::ObjectRoutine => "routine",
                        _ => "function",
                    };
                    match self.find_function(owa)? {
                        Some(f) => {
                            if objtype == O::ObjectProcedure && !f.procedure {
                                return Err(Error::new(
                                    code::WRONG_OBJECT_TYPE,
                                    format!("{} is not a procedure", self.function_sig(owa)),
                                ));
                            }
                            if objtype == O::ObjectFunction && f.procedure {
                                return Err(Error::new(
                                    code::WRONG_OBJECT_TYPE,
                                    format!("{} is not a function", self.function_sig(owa)),
                                ));
                            }
                            out.push(self.func_target(&f));
                        }
                        None => {
                            // A built-in one the function registry has, or any of pg_proc's
                            // (cast functions, …).
                            let def = match self.builtin_function(owa)? {
                                Some(d) => Some((d.oid, d.name.to_string())),
                                None => self.builtin_proc(owa)?,
                            };
                            let (oid, name) = def.ok_or_else(|| {
                                Error::new(
                                    code::UNDEFINED_FUNCTION,
                                    format!("{word} {} does not exist", self.function_sig(owa)),
                                )
                            })?;
                            out.push(Target {
                                kind: ObjKind::Function,
                                oid,
                                name,
                                owner: self.cat.builtin_proc_owners.get(&oid).copied().unwrap_or(BOOTSTRAP_SUPERUSER_OID),
                                relkind: ' ',
                                what: "function",
                            });
                        }
                    }
                }
                (O::ObjectType | O::ObjectDomain, Some(NodeEnum::List(l))) => {
                    let tn = pb::TypeName {
                        names: l.items.clone(),
                        typemod: -1,
                        ..Default::default()
                    };
                    out.push(self.grant_type_target(&tn, objtype == O::ObjectDomain)?);
                }
                (O::ObjectType | O::ObjectDomain, Some(NodeEnum::TypeName(tn))) => {
                    out.push(self.grant_type_target(tn, objtype == O::ObjectDomain)?)
                }
                (O::ObjectFdw | O::ObjectForeignServer, _) => {
                    let name = const_str(n).unwrap_or_default();
                    let server = objtype == O::ObjectForeignServer;
                    let found = if server {
                        self.server_named(&name).map(|x| (x.oid, x.owner))
                    } else {
                        self.fdw_named(&name).map(|x| (x.oid, x.owner))
                    }?;
                    out.push(Target {
                        kind: if server {
                            ObjKind::ForeignServer
                        } else {
                            ObjKind::ForeignDataWrapper
                        },
                        oid: found.0,
                        name,
                        owner: found.1,
                        relkind: ' ',
                        what: if server {
                            "foreign server"
                        } else {
                            "foreign-data wrapper"
                        },
                    });
                }
                (O::ObjectTablespace, _) => {
                    let name = const_str(n).unwrap_or_default();
                    let Some(oid) = self.cat.tablespace_oid(&name) else {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("tablespace \"{name}\" does not exist"),
                        ));
                    };
                    out.push(Target {
                        kind: ObjKind::Tablespace,
                        oid,
                        name,
                        owner: self.cat.tablespace_owner(oid),
                        relkind: ' ',
                        what: "tablespace",
                    });
                }
                (O::ObjectLanguage, _) => {
                    let name = const_str(n).unwrap_or_default();
                    let Some(lang) = self.cat.language_by_name(&name) else {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("language \"{name}\" does not exist"),
                        ));
                    };
                    let oid = &lang.oid;
                    if !lang.trusted {
                        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("language \"{name}\" is not trusted"))
                            .detail("GRANT and REVOKE are not allowed on untrusted languages, because only superusers can use untrusted languages."));
                    }
                    out.push(Target {
                        kind: ObjKind::Language,
                        oid: *oid,
                        name,
                        owner: BOOTSTRAP_SUPERUSER_OID,
                        relkind: ' ',
                        what: "language",
                    });
                }
                (O::ObjectParameterAcl, _) => {
                    // Known parameters by their name folded to lower case; others need a
                    // custom (dotted) name.
                    let name = const_str(n).unwrap_or_default();
                    if !crate::session::settings::is_builtin(&name) && !name.contains('.') {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("unrecognized configuration parameter \"{name}\""),
                        ));
                    }
                    let name = name.to_ascii_lowercase();
                    out.push(Target {
                        kind: ObjKind::Parameter,
                        oid: self
                            .cat
                            .parameters
                            .iter()
                            .find(|p| p.1 == name)
                            .map_or(0, |p| p.0),
                        name,
                        owner: BOOTSTRAP_SUPERUSER_OID,
                        relkind: ' ',
                        what: "parameter",
                    });
                }
                (O::ObjectLargeobject, _) => {
                    let oid = node_oid(Some(n), self.ctx.fmt())?;
                    let m = self.lo_meta(oid)?;
                    // Postgres names it "large object N" in both the warnings and the errors.
                    out.push(Target {
                        kind: ObjKind::LargeObject,
                        oid,
                        name: format!("large object {oid}"),
                        owner: m.owner,
                        relkind: ' ',
                        what: "large object",
                    });
                }
                _ => return Err(crate::error::unsupported("GRANT on this kind of object")),
            }
        }
        Ok(out)
    }

    /// The built-in function an ObjectWithArgs names (pg_catalog's, by its exact argument
    /// types; any one of that name without an argument list).
    fn builtin_function(
        &self,
        owa: &pb::ObjectWithArgs,
    ) -> Result<Option<Arc<crate::sql::func::FuncDef>>> {
        let parts = names(&owa.objname);
        if parts.len() > 1 && parts[parts.len() - 2] != "pg_catalog" {
            return Ok(None);
        }
        let name = parts.last().cloned().unwrap_or_default();
        let Some(defs) = crate::sql::func::registry().funcs.get(name.as_str()) else {
            return Ok(None);
        };
        if owa.args_unspecified {
            return Ok(defs.first().cloned());
        }
        let mut want = Vec::new();
        for a in &owa.objargs {
            let Some(NodeEnum::TypeName(tn)) = inner(a) else {
                return Ok(None);
            };
            let (ty, dom) = resolve_type_name(&self.cat, self.search_path(), tn)?;
            want.push(dom.unwrap_or_else(|| ty.oid()));
        }
        Ok(defs
            .iter()
            .find(|d| d.args.iter().map(DataType::oid).eq(want.iter().copied()))
            .cloned())
    }

    /// A pg_proc function by name and argument types: (oid, name).
    fn builtin_proc(&self, owa: &pb::ObjectWithArgs) -> Result<Option<(u32, String)>> {
        let parts = names(&owa.objname);
        if parts.len() > 1 && parts[parts.len() - 2] != "pg_catalog" {
            return Ok(None);
        }
        let name = parts.last().cloned().unwrap_or_default();
        let mut want = Vec::new();
        for a in &owa.objargs {
            let Some(NodeEnum::TypeName(tn)) = inner(a) else {
                return Ok(None);
            };
            let (ty, dom) = resolve_type_name(&self.cat, self.search_path(), tn)?;
            want.push(dom.unwrap_or_else(|| ty.oid()));
        }
        Ok(crate::catalog::system::builtin_proc_sigs()
            .iter()
            .find(|p| p.name == name && (owa.args_unspecified || p.args == want))
            .map(|p| (p.oid, p.name.to_string())))
    }

    fn rel_target(&self, rel: &Relation) -> Target {
        match rel {
            Relation::Table(t) => Target {
                kind: ObjKind::Table,
                oid: t.oid,
                name: t.name.clone(),
                owner: t.owner,
                relkind: 'r',
                what: "table",
            },
            Relation::View(v) if v.materialized => Target {
                kind: ObjKind::Table,
                oid: v.oid,
                name: v.name.clone(),
                owner: v.owner,
                relkind: 'm',
                what: "materialized view",
            },
            Relation::View(v) => Target {
                kind: ObjKind::Table,
                oid: v.oid,
                name: v.name.clone(),
                owner: v.owner,
                relkind: 'v',
                what: "view",
            },
            Relation::Sequence(q) => Target {
                kind: ObjKind::Sequence,
                oid: q.oid,
                name: q.name.clone(),
                owner: q.owner,
                relkind: 'S',
                what: "sequence",
            },
            Relation::Index(i) => Target {
                kind: ObjKind::Table,
                oid: i.oid,
                name: i.name.clone(),
                owner: BOOTSTRAP_SUPERUSER_OID,
                relkind: 'i',
                what: "index",
            },
        }
    }

    fn func_target(&self, f: &crate::catalog::Function) -> Target {
        Target {
            kind: ObjKind::Function,
            oid: f.oid,
            name: f.name.clone(),
            owner: f.owner,
            relkind: ' ',
            what: if f.procedure { "procedure" } else { "function" },
        }
    }

    /// The type a GRANT or an ALTER TYPE ... OWNER names (LookupTypeName: a shell type too).
    fn grant_type_target(&self, tn: &pb::TypeName, domain: bool) -> Result<Target> {
        let (ty, dom) =
            crate::sql::binder::resolve_type_name_shell(&self.cat, self.search_path(), tn)?;
        let shown = names(&tn.names).join(".");
        if let Some(d) = dom.and_then(|d| self.cat.domains.get(&d)) {
            return Ok(Target {
                kind: ObjKind::Type,
                oid: d.oid,
                name: d.name.clone(),
                owner: d.owner,
                relkind: ' ',
                what: "domain",
            });
        }
        if domain {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{shown}\" is not a domain"),
            ));
        }
        if ty.is_array() || !tn.array_bounds.is_empty() {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                "cannot set privileges of array types",
            )
            .hint("Set the privileges of the element type instead."));
        }
        if matches!(ty, DataType::Multirange(_)) {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                "cannot set privileges of multirange types",
            )
            .hint("Set the privileges of the range type instead."));
        }
        match &ty {
            DataType::Enum(oid, ..) => {
                let e = &self.cat.enums[oid];
                Ok(Target {
                    kind: ObjKind::Type,
                    oid: e.oid,
                    name: e.name.clone(),
                    owner: e.owner,
                    relkind: ' ',
                    what: "type",
                })
            }
            DataType::Row(r) if r.oid != 0 => match self.cat.composites.get(&r.oid) {
                Some(c) => Ok(Target {
                    kind: ObjKind::Type,
                    oid: c.oid,
                    name: c.name.clone(),
                    owner: c.owner,
                    relkind: ' ',
                    what: "type",
                }),
                None => Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{shown}\" is a table's row type"),
                )),
            },
            DataType::Range(rt) | DataType::Multirange(rt)
                if self.cat.ranges.contains_key(&rt.oid) =>
            {
                let r = &self.cat.ranges[&rt.oid];
                // A multirange's privileges are its range type's.
                Ok(Target {
                    kind: ObjKind::Type,
                    oid: r.oid,
                    name: r.name.clone(),
                    owner: r.owner,
                    relkind: ' ',
                    what: "type",
                })
            }
            DataType::Base(b) if self.cat.base_types.contains_key(&b.oid) => {
                let d = &self.cat.base_types[&b.oid];
                Ok(Target {
                    kind: ObjKind::Type,
                    oid: b.oid,
                    name: b.name.to_string(),
                    owner: d.owner,
                    relkind: ' ',
                    what: "type",
                })
            }
            // Built-in types: everyone may use them.
            t => Ok(Target {
                kind: ObjKind::Type,
                oid: t.oid(),
                name: t.display(),
                owner: BOOTSTRAP_SUPERUSER_OID,
                relkind: 'b',
                what: "type",
            }),
        }
    }

    // ── ALTER DEFAULT PRIVILEGES ──

    pub(super) fn alter_default_privileges(
        &mut self,
        s: &pb::AlterDefaultPrivilegesStmt,
    ) -> Result<String> {
        let me = self.me();
        let mut roles = Vec::new();
        let mut schemas = Vec::new();
        for d in def_elems(&s.options) {
            match d.defname.as_str() {
                "roles" => {
                    roles = d
                        .arg
                        .as_deref()
                        .map_or(Ok(Vec::new()), |a| self.node_roles(a))?
                }
                "schemas" => {
                    if let Some(NodeEnum::List(l)) = d.arg.as_deref().and_then(inner) {
                        for n in &l.items {
                            let name = const_str(n).unwrap_or_default();
                            schemas.push(self.cat.schema_oid(&name)?);
                        }
                    }
                }
                _ => {}
            }
        }
        if roles.is_empty() {
            roles.push(me);
        }
        let Some(g) = s.action.as_ref() else {
            return Ok("ALTER DEFAULT PRIVILEGES".into());
        };
        use pb::ObjectType as O;
        let objtype = O::try_from(g.objtype).unwrap_or(O::ObjectTable);
        let (letter, kind) = match objtype {
            O::ObjectTable => ('r', ObjKind::Table),
            O::ObjectSequence => ('S', ObjKind::Sequence),
            O::ObjectFunction | O::ObjectRoutine | O::ObjectProcedure => ('f', ObjKind::Function),
            O::ObjectType => ('T', ObjKind::Type),
            O::ObjectSchema => ('n', ObjKind::Schema),
            O::ObjectLargeobject => ('L', ObjKind::LargeObject),
            _ => {
                return Err(crate::error::unsupported(
                    "ALTER DEFAULT PRIVILEGES for this object type",
                ));
            }
        };
        if letter == 'n' && !schemas.is_empty() {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS",
            ));
        }
        if letter == 'L' && !schemas.is_empty() {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                "cannot use IN SCHEMA clause when using GRANT/REVOKE ON LARGE OBJECTS",
            ));
        }
        let (all_privs, errword) = grant_kind(objtype)?;
        let mut privs = 0;
        for p in &g.privileges {
            let Some(NodeEnum::AccessPriv(a)) = inner(p) else {
                continue;
            };
            let bit = acl::privilege_by_name(&a.priv_name).ok_or_else(|| {
                Error::new(
                    code::SYNTAX_ERROR,
                    format!("unrecognized privilege type \"{}\"", a.priv_name),
                )
            })?;
            if bit & !all_privs != 0 || bit & !kind.all() != 0 {
                return Err(Error::new(
                    code::INVALID_GRANT_OPERATION,
                    format!(
                        "invalid privilege type {} for {errword}",
                        acl::privilege_name(bit)
                    ),
                ));
            }
            privs |= bit;
        }
        if g.privileges.is_empty() {
            privs = kind.all();
        }
        let grantees = self.role_list(&g.grantees)?;
        if g.is_grant && g.grant_option && grantees.contains(&0) {
            return Err(Error::new(
                code::INVALID_GRANT_OPERATION,
                "grant options can only be granted to roles",
            ));
        }
        let cascade = g.behavior == pb::DropBehavior::DropCascade as i32;
        let db_owner = self.db_owner();
        for role in roles {
            if !self.cat.has_privs_of_role(me, role, db_owner) {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "permission denied to change default privileges",
                ));
            }
            let targets: Vec<u32> = if schemas.is_empty() {
                vec![0]
            } else {
                schemas.clone()
            };
            for schema in targets {
                let pos = self
                    .cat
                    .default_acls
                    .iter()
                    .position(|d| d.role == role && d.schema == schema && d.objtype == letter);
                let old = match pos {
                    Some(i) => self.cat.default_acls[i].acl.clone(),
                    None if schema == 0 => acl::default_acl(kind, role),
                    None => Vec::new(),
                };
                let mut new = old.clone();
                for &gr in &grantees {
                    let (p, o) = if g.is_grant {
                        (privs, if g.grant_option { privs } else { 0 })
                    } else if g.grant_option {
                        (0, privs)
                    } else {
                        (privs, privs)
                    };
                    acl::acl_update(&mut new, gr, role, p, o, g.is_grant, role, cascade)?;
                }
                if let Some(i) = pos {
                    self.cat.default_acls.remove(i);
                }
                let trivial = if schema == 0 {
                    same_acl(&new, &acl::default_acl(kind, role))
                } else {
                    new.is_empty()
                };
                if !trivial {
                    let oid = match pos {
                        Some(_) => self.cat.alloc_oid(),
                        None => self.cat.alloc_oid(),
                    };
                    self.cat.default_acls.push(DefaultAcl {
                        oid,
                        role,
                        schema,
                        objtype: letter,
                        acl: new,
                    });
                }
            }
        }
        self.save()?;
        Ok("ALTER DEFAULT PRIVILEGES".into())
    }

    /// Objects created by this statement start with the default privileges their owner set.
    pub(super) fn apply_default_privileges(&mut self, before: &crate::catalog::Catalog) {
        if self.cat.default_acls.is_empty() {
            return;
        }
        let mut new: Vec<(ObjKind, char, u32, u32, u32)> = Vec::new();
        for t in self
            .cat
            .tables
            .values()
            .filter(|t| !before.tables.contains_key(&t.oid))
        {
            new.push((ObjKind::Table, 'r', t.oid, t.owner, t.schema));
        }
        for v in self
            .cat
            .views
            .values()
            .filter(|v| !before.views.contains_key(&v.oid))
        {
            new.push((ObjKind::Table, 'r', v.oid, v.owner, v.schema));
        }
        for q in self
            .cat
            .sequences
            .values()
            .filter(|q| !before.sequences.contains_key(&q.oid))
        {
            new.push((ObjKind::Sequence, 'S', q.oid, q.owner, q.schema));
        }
        for f in self
            .cat
            .functions
            .values()
            .filter(|f| !before.functions.contains_key(&f.oid))
        {
            new.push((ObjKind::Function, 'f', f.oid, f.owner, f.schema));
        }
        for e in self
            .cat
            .enums
            .values()
            .filter(|e| !before.enums.contains_key(&e.oid))
        {
            new.push((ObjKind::Type, 'T', e.oid, e.owner, e.schema));
        }
        for e in self
            .cat
            .composites
            .values()
            .filter(|e| !before.composites.contains_key(&e.oid))
        {
            new.push((ObjKind::Type, 'T', e.oid, e.owner, e.schema));
        }
        for e in self
            .cat
            .ranges
            .values()
            .filter(|e| !before.ranges.contains_key(&e.oid))
        {
            new.push((ObjKind::Type, 'T', e.oid, e.owner, e.schema));
        }
        for e in self
            .cat
            .domains
            .values()
            .filter(|e| !before.domains.contains_key(&e.oid))
        {
            new.push((ObjKind::Type, 'T', e.oid, e.owner, e.schema));
        }
        for sc in self
            .cat
            .schemas
            .values()
            .filter(|s| !before.schemas.contains_key(&s.oid) && !s.name.starts_with("pg_temp"))
        {
            new.push((ObjKind::Schema, 'n', sc.oid, sc.owner, 0));
        }
        for (kind, letter, oid, owner, schema) in new {
            if self.cat.acl(kind, oid, 0).is_some() {
                continue;
            }
            let global = self
                .cat
                .default_acls
                .iter()
                .find(|d| d.role == owner && d.schema == 0 && d.objtype == letter)
                .map(|d| d.acl.clone());
            let local = if schema != 0 {
                self.cat
                    .default_acls
                    .iter()
                    .find(|d| d.role == owner && d.schema == schema && d.objtype == letter)
                    .map(|d| d.acl.clone())
            } else {
                None
            };
            if global.is_none() && local.is_none() {
                continue;
            }
            let mut acl = global.unwrap_or_else(|| acl::default_acl(kind, owner));
            for item in local.unwrap_or_default() {
                match acl
                    .iter_mut()
                    .find(|i| i.grantee == item.grantee && i.grantor == item.grantor)
                {
                    Some(i) => {
                        i.privs |= item.privs;
                        i.goptions |= item.goptions;
                    }
                    None => acl.push(item),
                }
            }
            if !same_acl(&acl, &acl::default_acl(kind, owner)) {
                self.cat.set_acl(kind, oid, 0, Some(acl));
            }
        }
    }

    /// Forget the privileges of objects dropped by this statement (pg_shdepend cleanup).
    pub(super) fn prune_acls(&mut self, before: &crate::catalog::Catalog) {
        fn gone<V>(
            a: &std::collections::BTreeMap<u32, V>,
            b: &std::collections::BTreeMap<u32, V>,
            out: &mut Vec<u32>,
        ) {
            out.extend(a.keys().filter(|k| !b.contains_key(k)));
        }
        // (A shared map: only the entries that changed.)
        fn gone_p<V>(
            a: &crate::catalog::pmap::PMap<std::sync::Arc<V>>,
            b: &crate::catalog::pmap::PMap<std::sync::Arc<V>>,
            out: &mut Vec<u32>,
        ) {
            out.extend(a.changes(b).into_iter().filter(|(_, _, n)| n.is_none()).map(|(k, _, _)| k));
        }
        let mut rels = Vec::new();
        gone_p(&before.tables, &self.cat.tables, &mut rels);
        gone_p(&before.views, &self.cat.views, &mut rels);
        gone_p(&before.sequences, &self.cat.sequences, &mut rels);
        let mut types = Vec::new();
        gone(&before.enums, &self.cat.enums, &mut types);
        gone(&before.composites, &self.cat.composites, &mut types);
        gone(&before.domains, &self.cat.domains, &mut types);
        gone(&before.ranges, &self.cat.ranges, &mut types);
        let mut funcs = Vec::new();
        gone_p(&before.functions, &self.cat.functions, &mut funcs);
        let mut schemas = Vec::new();
        gone(&before.schemas, &self.cat.schemas, &mut schemas);
        let tables = &self.cat.tables;
        self.cat
            .policies
            .retain(|_, p| tables.contains_key(&p.table));
        let views = &self.cat.views;
        self.cat
            .triggers
            .retain(|_, x| tables.contains_key(&x.table) || views.contains_key(&x.table));
        self.cat.acls.retain(|a| {
            !(a.class == 1259
                && a.sub > 0
                && tables
                    .get(&a.oid)
                    .is_some_and(|t| t.columns.get(a.sub as usize - 1).is_none_or(|c| c.dropped)))
        });
        if rels.is_empty() && types.is_empty() && funcs.is_empty() && schemas.is_empty() {
            return;
        }
        let dead = |class: u32, oid: u32| match class {
            1259 => rels.contains(&oid),
            1247 => types.contains(&oid),
            1255 => funcs.contains(&oid),
            2615 => schemas.contains(&oid),
            _ => false,
        };
        self.cat.acls.retain(|a| !dead(a.class, a.oid));
        self.cat
            .default_acls
            .retain(|d| !schemas.contains(&d.schema));
    }

    // ── ownership ──

    /// Fail unless the current user owns (is a member of the owner of) the object.
    pub(super) fn require_owner(&self, owner: u32, what: &str, name: &str) -> Result<()> {
        let me = self.me();
        if self.cat.has_privs_of_role(me, owner, self.db_owner()) {
            return Ok(());
        }
        Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of {what} {name}"),
        ))
    }

    /// Ownership of an object or of its schema: what DROP needs.
    pub(super) fn require_owner_or_schema(
        &self,
        owner: u32,
        schema: u32,
        what: &str,
        name: &str,
    ) -> Result<()> {
        if let Some(s) = self.cat.schemas.get(&schema)
            && self
                .cat
                .has_privs_of_role(self.me(), s.owner, self.db_owner())
        {
            return Ok(());
        }
        self.require_owner(owner, what, name)
    }

    /// Ownership of a relation (an index's table), or of its schema: what DROP needs.
    pub(super) fn require_rel_owner_or_schema(
        &self,
        oid: u32,
        what: &str,
        name: &str,
    ) -> Result<()> {
        let Some((owner, _)) = self.cat.rel_owner(oid) else {
            return Ok(());
        };
        let me = self.me();
        if self.cat.has_privs_of_role(me, owner, self.db_owner()) {
            return Ok(());
        }
        let schema = self.rel_schema(oid);
        if let Some(s) = schema.and_then(|s| self.cat.schemas.get(&s))
            && self.cat.has_privs_of_role(me, s.owner, self.db_owner())
        {
            return Ok(());
        }
        Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of {what} {name}"),
        ))
    }

    /// Ownership of a relation (an index's table): what ALTER, RENAME, … need.
    pub(super) fn require_rel_owner(&self, oid: u32, what: &str, name: &str) -> Result<()> {
        match self.cat.rel_owner(oid) {
            Some((owner, _)) => self.require_owner(owner, what, name),
            None => Ok(()),
        }
    }

    pub(super) fn rel_schema(&self, oid: u32) -> Option<u32> {
        if let Some(t) = self.cat.tables.get(&oid) {
            return Some(t.schema);
        }
        if let Some(v) = self.cat.views.get(&oid) {
            return Some(v.schema);
        }
        if let Some(s) = self.cat.sequences.get(&oid) {
            return Some(s.schema);
        }
        self.cat
            .indexes
            .get(&oid)
            .and_then(|i| self.cat.index_table(i.table))
            .map(|t| t.schema)
    }

    /// CREATE on a schema (to create objects in it).
    /// LookupExplicitNamespace: names qualified with a schema need USAGE on it.
    pub(super) fn require_schema_usage(&self, schema: &str) -> Result<()> {
        let me = self.me();
        if self.cat.is_superuser(me) {
            return Ok(());
        }
        let Some(s) = self.cat.schema_by_name(schema) else {
            return Ok(());
        };
        if s.name.starts_with("pg_temp") {
            return Ok(());
        }
        let (p, _) = self
            .cat
            .obj_mask(ObjKind::Schema, s.oid, me, self.db_owner());
        if p & acl::USAGE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for schema {schema}"),
            ));
        }
        Ok(())
    }

    pub(super) fn require_schema_create(&self, schema: u32) -> Result<()> {
        let me = self.me();
        if self.cat.is_superuser(me) {
            return Ok(());
        }
        let Some(s) = self.cat.schemas.get(&schema) else {
            return Ok(());
        };
        if s.name.starts_with("pg_temp") {
            return Ok(());
        }
        let (p, _) = self
            .cat
            .obj_mask(ObjKind::Schema, schema, me, self.db_owner());
        if p & acl::CREATE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for schema {}", s.name),
            ));
        }
        Ok(())
    }

    /// A privilege on the current database (CREATE, TEMP, CONNECT).
    pub(super) fn require_database_priv(&self, privs: u32) -> Result<()> {
        let me = self.me();
        if self.cat.is_superuser(me) {
            return Ok(());
        }
        let db = &self.ctx.sess.database;
        let Some((oid, _, _)) = self
            .ctx
            .sess
            .cluster
            .databases()
            .into_iter()
            .find(|d| &d.1 == db)
        else {
            return Ok(());
        };
        let (p, _) = self
            .cat
            .obj_mask(ObjKind::Database, oid, me, self.db_owner());
        if p & privs == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for database {}", self.ctx.sess.database),
            ));
        }
        Ok(())
    }

    /// TEMP on the current database, to create a temporary table: checked when the session's
    /// temporary schema is set up, then as CREATE on that schema.
    pub(super) fn require_temp_create(&self) -> Result<()> {
        let name = self.ctx.sess.temp_schema();
        if self.cat.schema_by_name(&name).is_some() && self.require_temp().is_err() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for schema {name}"),
            ));
        }
        self.require_temp()
    }

    pub(super) fn require_temp(&self) -> Result<()> {
        self.require_database_priv(acl::TEMPORARY).map_err(|_| {
            Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "permission denied to create temporary tables in database \"{}\"",
                    self.ctx.sess.database
                ),
            )
        })
    }

    /// Privileges on a relation, as the current user (all of them, or `any` one).
    pub(super) fn require_rel_priv(&self, oid: u32, privs: u32, any: bool) -> Result<()> {
        let me = self.me();
        if self.cat.is_superuser(me) {
            return Ok(());
        }
        let Some((p, _)) = self.cat.rel_mask(oid, me, self.db_owner()) else {
            return Ok(());
        };
        if if any {
            p & privs == 0
        } else {
            p & privs != privs
        } {
            let (word, name) = crate::sql::binder::perms::rel_word(&self.cat, oid);
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {word} {name}"),
            ));
        }
        Ok(())
    }

    /// The checks of an ownership change (besides ownership itself): the current user must be
    /// able to SET ROLE to the new owner, which must be able to create in the schema.
    pub(super) fn check_new_owner(&self, new_owner: u32, schema: Option<u32>) -> Result<()> {
        let me = self.me();
        if self.cat.is_superuser(me) {
            return Ok(());
        }
        if !self.cat.can_set_role(me, new_owner) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be able to SET ROLE \"{}\"", self.role_name(new_owner)),
            ));
        }
        if let Some(sc) = schema
            && let Some(s) = self.cat.schemas.get(&sc)
        {
            let acl = self.cat.effective_acl(ObjKind::Schema, sc, s.owner);
            let (p, _) = self.cat.aclmask(&acl, new_owner, s.owner, self.db_owner());
            if p & acl::CREATE == 0 {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", s.name),
                ));
            }
        }
        Ok(())
    }

    /// aclnewowner on an object's stored ACLs (its columns' too).
    pub(super) fn reown_acls(&mut self, kind: ObjKind, oid: u32, old: u32, new: u32) {
        let class = kind.class_oid();
        for a in self
            .cat
            .acls
            .iter_mut()
            .filter(|a| a.class == class && a.oid == oid)
        {
            a.acl = new_owner_acl(&a.acl, old, new);
        }
    }

    /// ALTER <object> OWNER TO for everything but tables (ALTER TABLE handles those).
    pub(super) fn alter_owner(&mut self, s: &pb::AlterOwnerStmt) -> Result<String> {
        use pb::ObjectType as O;
        let kind = O::try_from(s.object_type).unwrap_or(O::ObjectTable);
        let tag = format!("ALTER {}", super::object_type_word(kind));
        let new = self.role_oid(s.newowner.as_ref().unwrap())?;
        let obj = s.object.as_deref();
        match kind {
            O::ObjectEventTrigger => {
                let name = obj.and_then(const_str).unwrap_or_default();
                return self.event_trigger_owner(&name, new);
            }
            O::ObjectSchema => {
                let name = obj.and_then(const_str).unwrap_or_default();
                let sc = self.cat.schema_by_name(&name).cloned().ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_SCHEMA,
                        format!("schema \"{name}\" does not exist"),
                    )
                })?;
                if sc.owner != new {
                    self.require_owner(sc.owner, "schema", &name)?;
                    self.check_new_owner(new, None)?;
                    if !self.is_superuser() {
                        // The new owner needs CREATE on the database.
                    }
                    self.reown_acls(ObjKind::Schema, sc.oid, sc.owner, new);
                    self.cat.schemas.get_mut(&sc.oid).unwrap().owner = new;
                }
            }
            O::ObjectOperator => {
                let Some(NodeEnum::ObjectWithArgs(owa)) = obj.and_then(inner) else {
                    return Ok(tag);
                };
                self.operator_owner(owa, new)?;
            }
            O::ObjectTsconfiguration | O::ObjectTsdictionary => {
                self.owner_ts(super::tsearch::ts_kind(kind).unwrap(), obj, new)?;
            }
            O::ObjectCollation => {
                let parts = match obj.and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                self.collation_owner(&parts, new)?;
            }
            O::ObjectStatisticExt => {
                let parts = match obj.and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                let Some(st) = self.statistics_object(&parts, false)? else {
                    return Ok(tag);
                };
                if st.owner != new {
                    self.require_owner(st.owner, "statistics object", &st.name)?;
                    self.check_new_owner(new, Some(st.schema))?;
                    let mut x = (*st).clone();
                    x.owner = new;
                    self.cat.statistics.insert(x.oid, Arc::new(x));
                }
            }
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                let Some(NodeEnum::ObjectWithArgs(owa)) = obj.and_then(inner) else {
                    return Ok(tag);
                };
                let Some(f) = self.find_routine(owa, kind)? else {
                    // A built-in one (AlterFunctionOwner works on initdb's routines too):
                    // pg_proc.proowner, and its ACL's owner entries.
                    let def = match self.builtin_function(owa)? {
                        Some(d) => Some((d.oid, d.name.to_string())),
                        None => self.builtin_proc(owa)?,
                    };
                    let Some((oid, name)) = def else {
                        return Err(Error::new(
                            code::UNDEFINED_FUNCTION,
                            format!(
                                "{} {} does not exist",
                                super::object_type_word(kind).to_lowercase(),
                                self.function_sig(owa)
                            ),
                        ));
                    };
                    let old = self
                        .cat
                        .builtin_proc_owners
                        .get(&oid)
                        .copied()
                        .unwrap_or(BOOTSTRAP_SUPERUSER_OID);
                    if old != new {
                        self.require_owner(old, "function", &name)?;
                        self.check_new_owner(new, Some(crate::catalog::PG_CATALOG_OID))?;
                        let stored = self.cat.acl(ObjKind::Function, oid, 0).cloned();
                        if let Some(acl) = stored.or_else(|| acl::builtin_proc_acl(oid)) {
                            let acl = new_owner_acl(&acl, old, new);
                            self.cat.set_acl(ObjKind::Function, oid, 0, Some(acl));
                        }
                        if new == BOOTSTRAP_SUPERUSER_OID {
                            self.cat.builtin_proc_owners.remove(&oid);
                        } else {
                            self.cat.builtin_proc_owners.insert(oid, new);
                        }
                    }
                    self.save()?;
                    return Ok(tag);
                };
                if f.owner != new {
                    self.require_owner(
                        f.owner,
                        if f.procedure { "procedure" } else { "function" },
                        &f.name,
                    )?;
                    self.check_new_owner(new, Some(f.schema))?;
                    self.reown_acls(ObjKind::Function, f.oid, f.owner, new);
                    let mut nf = (*f).clone();
                    nf.owner = new;
                    self.cat.functions.insert(f.oid, Arc::new(nf));
                }
            }
            O::ObjectType | O::ObjectDomain => {
                let tn = match obj.and_then(inner) {
                    Some(NodeEnum::List(l)) => pb::TypeName {
                        names: l.items.clone(),
                        typemod: -1,
                        ..Default::default()
                    },
                    Some(NodeEnum::TypeName(tn)) => tn.clone(),
                    _ => return Ok(tag),
                };
                // A multirange goes with its range type (AlterTypeOwner).
                if let Ok((DataType::Multirange(rt), None)) =
                    resolve_type_name(&self.cat, self.search_path(), &tn)
                {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("cannot alter multirange type {}", rt.multi_name),
                    )
                    .hint(format!(
                        "You can alter type {}, which will alter the multirange type as well.",
                        rt.name
                    )));
                }
                let t = self.grant_type_target(&tn, kind == O::ObjectDomain)?;
                if t.relkind == 'b' {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("must be owner of type {}", t.name),
                    ));
                }
                if t.owner != new {
                    self.require_owner(t.owner, t.what, &t.name)?;
                    let schema = self.cat.user_type_name(t.oid).map(|(s, _, _)| s);
                    self.check_new_owner(new, schema)?;
                    self.reown_acls(ObjKind::Type, t.oid, t.owner, new);
                    if let Some(e) = self.cat.enums.get(&t.oid).cloned() {
                        self.cat.enums.insert(
                            t.oid,
                            Arc::new(crate::catalog::EnumType {
                                owner: new,
                                ..(*e).clone()
                            }),
                        );
                    } else if let Some(c) = self.cat.composites.get(&t.oid).cloned() {
                        self.cat.composites.insert(
                            t.oid,
                            Arc::new(crate::catalog::CompositeType {
                                owner: new,
                                ..(*c).clone()
                            }),
                        );
                    } else if let Some(d) = self.cat.domains.get(&t.oid).cloned() {
                        self.cat.domains.insert(
                            t.oid,
                            Arc::new(crate::catalog::Domain {
                                owner: new,
                                ..(*d).clone()
                            }),
                        );
                    } else if let Some(r) = self.cat.ranges.get(&t.oid).cloned() {
                        self.cat.ranges.insert(
                            t.oid,
                            Arc::new(crate::catalog::RangeDef {
                                owner: new,
                                ..(*r).clone()
                            }),
                        );
                    }
                }
            }
            O::ObjectOpclass | O::ObjectOpfamily => {
                self.owner_opc(kind == O::ObjectOpfamily, obj, new)?;
            }
            O::ObjectConversion | O::ObjectLanguage => {
                self.owner_misc(kind, obj, new)?;
            }
            O::ObjectFdw | O::ObjectForeignServer => {
                self.owner_fdw(kind == O::ObjectForeignServer, obj, new)?;
            }
            O::ObjectPublication => {
                let name = obj.and_then(const_str).unwrap_or_default();
                self.owner_publication(&name, new)?;
            }
            O::ObjectSubscription => {
                let name = obj.and_then(const_str).unwrap_or_default();
                self.owner_subscription(&name, new)?;
            }
            O::ObjectTablespace => {
                let name = obj.and_then(const_str).unwrap_or_default();
                let oid = self.tablespace_named(&name)?;
                let owner = self.cat.tablespace_owner(oid);
                if owner != new {
                    self.require_owner(owner, "tablespace", &name)?;
                    self.check_new_owner(new, None)?;
                    self.set_tablespace_owner(oid, new);
                }
            }
            O::ObjectLargeobject => {
                let oid = node_oid(obj, self.ctx.fmt())?;
                let mut m = self.lo_meta(oid)?;
                if m.owner != new {
                    self.require_owner(m.owner, "large object", &oid.to_string())?;
                    self.check_new_owner(new, None)?;
                    m.acl = m.acl.map(|a| new_owner_acl(&a, m.owner, new));
                    m.owner = new;
                    self.lo_put(oid, &m)?;
                }
            }
            O::ObjectDatabase => {
                let name = obj.and_then(const_str).unwrap_or_default();
                if !self
                    .ctx
                    .sess
                    .cluster
                    .databases()
                    .iter()
                    .any(|d| d.1 == name)
                {
                    return Err(Error::new(
                        code::UNDEFINED_DATABASE,
                        format!("database \"{name}\" does not exist"),
                    ));
                }
                let owner = self
                    .ctx
                    .sess
                    .cluster
                    .database_owner(&name)
                    .and_then(|o| self.cat.role_by_name(&o).map(|r| r.oid))
                    .unwrap_or(BOOTSTRAP_SUPERUSER_OID);
                if owner != new {
                    self.require_owner(owner, "database", &name)?;
                    self.check_new_owner(new, None)?;
                    if !self.is_superuser()
                        && !self.cat.roles.get(&self.me()).is_some_and(|r| r.create_db)
                    {
                        return Err(Error::new(
                            code::INSUFFICIENT_PRIVILEGE,
                            "permission denied to change owner of database",
                        ));
                    }
                    let new_name = self.role_name(new);
                    if let Some(old) = self.ctx.sess.cluster.database_owner(&name) {
                        self.ctx
                            .txn
                            .borrow_mut()
                            .db_owner_undo
                            .push((name.to_string(), old));
                    }
                    self.ctx.sess.cluster.set_database_owner(&name, &new_name)?;
                }
            }
            _ => {}
        }
        self.save()?;
        Ok(tag)
    }

    /// Change a relation's owner (ALTER TABLE / VIEW / SEQUENCE … OWNER TO).
    pub(super) fn change_rel_owner(
        &mut self,
        oid: u32,
        owner: u32,
        schema: u32,
        what: &str,
        name: &str,
        new: u32,
    ) -> Result<()> {
        if owner == new {
            return Ok(());
        }
        self.require_owner(owner, what, name)?;
        self.check_new_owner(new, Some(schema))?;
        self.reown_acls(ObjKind::Table, oid, owner, new);
        Ok(())
    }

    // ── REASSIGN OWNED / DROP OWNED ──

    pub(super) fn reassign_owned(&mut self, s: &pb::ReassignOwnedStmt) -> Result<String> {
        let me = self.me();
        let db_owner = self.db_owner();
        let old = self.role_list(&s.roles)?;
        let new = self.role_oid(s.newrole.as_ref().unwrap())?;
        for &r in &old {
            if !self.cat.has_privs_of_role(me, r, db_owner) {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "permission denied to reassign objects",
                )
                .detail(format!(
                    "Only roles with privileges of role \"{}\" may reassign objects owned by it.",
                    self.role_name(r)
                )));
            }
        }
        if !self.cat.has_privs_of_role(me, new, db_owner) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied to reassign objects",
            )
            .detail(format!(
                "Only roles with privileges of role \"{}\" may reassign objects to it.",
                self.role_name(new)
            )));
        }
        for (kind, oid, owner) in self.owned_objects() {
            if !old.contains(&owner) {
                continue;
            }
            self.reown_acls(kind, oid, owner, new);
            self.set_owner(kind, oid, new);
        }
        for (kind, oid, owner) in self.owned_ts() {
            if old.contains(&owner) {
                self.set_ts_owner(kind, oid, new);
            }
        }
        // Foreign servers and wrappers (user mappings stay with their user).
        let servers: Vec<_> = self
            .cat
            .servers
            .values()
            .filter(|x| old.contains(&x.owner))
            .cloned()
            .collect();
        for x in servers {
            self.reown_acls(ObjKind::ForeignServer, x.oid, x.owner, new);
            let mut x = (*x).clone();
            x.owner = new;
            self.cat.servers.insert(x.oid, Arc::new(x));
        }
        let fdws: Vec<_> = self
            .cat
            .fdws
            .values()
            .filter(|x| old.contains(&x.owner))
            .cloned()
            .collect();
        for x in fdws {
            self.reown_acls(ObjKind::ForeignDataWrapper, x.oid, x.owner, new);
            let mut x = (*x).clone();
            x.owner = new;
            self.cat.fdws.insert(x.oid, Arc::new(x));
        }
        // Tablespaces, subscriptions (shared) and publications.
        let spcs: Vec<u32> = self
            .cat
            .tablespaces
            .values()
            .filter(|t| old.contains(&t.owner))
            .map(|t| t.oid)
            .collect();
        for oid in spcs {
            self.set_tablespace_owner(oid, new);
        }
        for x in self.cat.subscriptions.values_mut() {
            if old.contains(&x.owner) {
                x.owner = new;
            }
        }
        let pubs: Vec<_> = self
            .cat
            .publications
            .values()
            .filter(|p| old.contains(&p.owner))
            .cloned()
            .collect();
        for p in pubs {
            let mut p = (*p).clone();
            p.owner = new;
            self.cat.publications.insert(p.oid, Arc::new(p));
        }
        // Owned sequences follow their tables.
        for q in self
            .cat
            .sequences
            .values()
            .filter(|q| q.owned_by.is_some() && old.contains(&q.owner))
            .map(|q| q.oid)
            .collect::<Vec<_>>()
        {
            self.set_owner(ObjKind::Sequence, q, new);
        }
        // (Default privileges stay: DROP OWNED handles them, not REASSIGN OWNED.)
        for (oid, mut m) in self.large_objects()? {
            if old.contains(&m.owner) {
                m.acl = m.acl.map(|a| new_owner_acl(&a, m.owner, new));
                m.owner = new;
                self.lo_put(oid, &m)?;
            }
        }
        self.save()?;
        Ok("REASSIGN OWNED".into())
    }

    fn set_ts_owner(&mut self, kind: crate::catalog::tsearch::TsKind, oid: u32, new: u32) {
        use crate::catalog::tsearch::TsKind;
        match kind {
            TsKind::Config => {
                if let Some(c) = self.cat.ts_config(oid) {
                    self.cat
                        .ts_put_config(crate::types::tsearch::config::TsConfigDef {
                            owner: new,
                            ..c
                        });
                }
            }
            TsKind::Dict => {
                if let Some(d) = self.cat.ts_dict(oid) {
                    self.cat
                        .ts_put_dict(crate::types::tsearch::config::TsDictDef { owner: new, ..d });
                }
            }
            _ => {}
        }
    }

    fn set_owner(&mut self, kind: ObjKind, oid: u32, new: u32) {
        let c = &mut self.cat;
        match kind {
            ObjKind::Schema => {
                if let Some(s) = c.schemas.get_mut(&oid) {
                    s.owner = new;
                }
            }
            ObjKind::Table | ObjKind::Sequence => {
                if let Some(t) = c.tables.get(&oid).cloned() {
                    let mut t = (*t).clone();
                    t.owner = new;
                    c.put_table(t);
                } else if let Some(v) = c.views.get(&oid).cloned() {
                    c.views.insert(
                        oid,
                        Arc::new(crate::catalog::ViewDef {
                            owner: new,
                            ..(*v).clone()
                        }),
                    );
                } else if let Some(q) = c.sequences.get(&oid).cloned() {
                    c.sequences.insert(
                        oid,
                        Arc::new(crate::catalog::Sequence {
                            owner: new,
                            ..(*q).clone()
                        }),
                    );
                }
            }
            ObjKind::Function => {
                if let Some(f) = c.functions.get(&oid).cloned() {
                    c.functions.insert(
                        oid,
                        Arc::new(crate::catalog::Function {
                            owner: new,
                            ..(*f).clone()
                        }),
                    );
                }
            }
            ObjKind::Type => {
                if let Some(e) = c.enums.get(&oid).cloned() {
                    c.enums.insert(
                        oid,
                        Arc::new(crate::catalog::EnumType {
                            owner: new,
                            ..(*e).clone()
                        }),
                    );
                } else if let Some(e) = c.composites.get(&oid).cloned() {
                    c.composites.insert(
                        oid,
                        Arc::new(crate::catalog::CompositeType {
                            owner: new,
                            ..(*e).clone()
                        }),
                    );
                } else if let Some(e) = c.domains.get(&oid).cloned() {
                    c.domains.insert(
                        oid,
                        Arc::new(crate::catalog::Domain {
                            owner: new,
                            ..(*e).clone()
                        }),
                    );
                } else if let Some(e) = c.ranges.get(&oid).cloned() {
                    c.ranges.insert(
                        oid,
                        Arc::new(crate::catalog::RangeDef {
                            owner: new,
                            ..(*e).clone()
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    pub(super) fn drop_owned(&mut self, s: &pb::DropOwnedStmt) -> Result<String> {
        let me = self.me();
        let db_owner = self.db_owner();
        let roles = self.role_list(&s.roles)?;
        for &r in &roles {
            if !self.cat.has_privs_of_role(me, r, db_owner) {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "permission denied to drop objects",
                )
                .detail(format!(
                    "Only roles with privileges of role \"{}\" may drop objects owned by it.",
                    self.role_name(r)
                )));
            }
        }
        const SP: &str = "\0drop_owned";
        // shdepDropOwned: each role membership they granted is locked for its deletion
        // (AcquireDeletionLock) and looked for again once that is held: one another DROP
        // OWNED removed meanwhile is gone from what this one then works over.
        let grants: Vec<u32> = self
            .cat
            .members
            .iter()
            .filter(|m| roles.contains(&m.grantor))
            .map(|m| m.oid)
            .collect();
        for oid in grants {
            self.lock_object(super::AUTH_MEMBERS_CLASS, oid, crate::txn::locks::LockMode::AccessExclusive)?;
        }
        self.reload_after_wait();
        self.ctx.txn.borrow_mut().savepoint(SP);
        let r = self.drop_owned_inner(&roles, s.behavior == pb::DropBehavior::DropCascade as i32);
        let mut txn = self.ctx.txn.borrow_mut();
        if r.is_err() {
            txn.rollback_to(SP)?;
        }
        txn.release_savepoint(SP)?;
        drop(txn);
        if r.is_err() {
            self.reload_catalog();
        }
        r
    }

    fn drop_owned_inner(&mut self, roles: &[u32], cascade: bool) -> Result<String> {
        // Privileges granted to them go (REVOKE ALL ... CASCADE: with the grants resting on
        // their grant options).
        let db_owner = self.db_owner();
        let owners: Vec<Option<u32>> = self
            .cat
            .acls
            .iter()
            .map(|a| self.cat.owner_by_class(a.class, a.oid, db_owner))
            .collect();
        for (a, owner) in self.cat.acls.iter_mut().zip(owners) {
            revoke_roles_cascade(&mut a.acl, roles, owner);
        }
        // Large objects they own go; the others lose their privileges.
        for (oid, mut m) in self.large_objects()? {
            if roles.contains(&m.owner) {
                let mut txn = self.ctx.txn.borrow_mut();
                txn.lo_close_all(oid);
                crate::largeobj::drop(&mut txn, oid)?;
            } else if let Some(acl) = &mut m.acl
                && acl.iter().any(|i| roles.contains(&i.grantee))
            {
                acl.retain(|i| !roles.contains(&i.grantee));
                self.lo_put(oid, &m)?;
            }
        }
        // Role memberships they granted go.
        self.cat.members.retain(|m| !roles.contains(&m.grantor));
        self.cat.default_acls.retain(|d| !roles.contains(&d.role));
        for d in &mut self.cat.default_acls {
            d.acl.retain(|i| !roles.contains(&i.grantee));
        }
        // Policies lose them as targets, and go if no target is left.
        let pols: Vec<Arc<crate::catalog::Policy>> = self
            .cat
            .policies
            .values()
            .filter(|p| p.roles.iter().any(|r| roles.contains(r)))
            .cloned()
            .collect();
        for p in pols {
            let mut p = (*p).clone();
            p.roles.retain(|r| !roles.contains(r));
            if p.roles.is_empty() {
                self.cat.policies.remove(&p.oid);
            } else {
                self.cat.policies.insert(p.oid, Arc::new(p));
            }
        }
        // Then the objects they own, as DROP … CASCADE would; without CASCADE, nothing else may
        // depend on them.
        let owned: Vec<(ObjKind, u32, u32)> = self
            .owned_objects()
            .into_iter()
            .filter(|o| roles.contains(&o.2))
            .collect();
        let mut stmts = Vec::new();
        let owned_ts: Vec<(crate::catalog::tsearch::TsKind, u32, u32)> = self
            .owned_ts()
            .into_iter()
            .filter(|o| roles.contains(&o.2))
            .collect();
        for (kind, oid, _) in &owned_ts {
            let (ns, name) = self.cat.ts_ident(*kind, *oid).unwrap_or_default();
            let sql = format!(
                "DROP {} IF EXISTS {}.{} CASCADE",
                kind.noun().to_uppercase(),
                crate::catalog::quote_ident(self.cat.schema_name(ns)),
                crate::catalog::quote_ident(&name)
            );
            stmts.push((*oid, sql, self.ts_description(*kind, *oid)));
        }
        for (kind, oid, _) in owned.iter().rev() {
            let c = &self.cat;
            let sql = match kind {
                ObjKind::Table => {
                    if let Some(t) = c.tables.get(oid) {
                        format!(
                            "DROP {}TABLE IF EXISTS {}.{}",
                            if t.is_foreign() { "FOREIGN " } else { "" },
                            crate::catalog::quote_ident(c.schema_name(t.schema)),
                            crate::catalog::quote_ident(&t.name)
                        )
                    } else if let Some(v) = c.views.get(oid) {
                        format!(
                            "DROP {}VIEW IF EXISTS {}.{}",
                            if v.materialized { "MATERIALIZED " } else { "" },
                            crate::catalog::quote_ident(c.schema_name(v.schema)),
                            crate::catalog::quote_ident(&v.name)
                        )
                    } else {
                        continue;
                    }
                }
                ObjKind::Sequence => match c.sequences.get(oid) {
                    Some(q) => format!(
                        "DROP SEQUENCE IF EXISTS {}.{}",
                        crate::catalog::quote_ident(c.schema_name(q.schema)),
                        crate::catalog::quote_ident(&q.name)
                    ),
                    None => continue,
                },
                ObjKind::Function => match c.functions.get(oid) {
                    Some(f) => format!(
                        "DROP {} IF EXISTS {}.{}({})",
                        if f.procedure {
                            "PROCEDURE"
                        } else if f.agg.is_some() {
                            "ROUTINE"
                        } else {
                            "FUNCTION"
                        },
                        crate::catalog::quote_ident(c.schema_name(f.schema)),
                        crate::catalog::quote_ident(&f.name),
                        c.param_type_names(f, self.search_path()).join(", ")
                    ),
                    None => continue,
                },
                ObjKind::Type => match c.user_type_name(*oid).or_else(|| {
                    c.domains
                        .get(oid)
                        .map(|d| (d.schema, d.name.as_str(), false))
                }) {
                    Some((s, n, _)) => format!(
                        "DROP {} IF EXISTS {}.{}",
                        if c.domains.contains_key(oid) {
                            "DOMAIN"
                        } else {
                            "TYPE"
                        },
                        crate::catalog::quote_ident(c.schema_name(s)),
                        crate::catalog::quote_ident(n)
                    ),
                    None => continue,
                },
                ObjKind::Schema => match c.schemas.get(oid) {
                    Some(s) => format!(
                        "DROP SCHEMA IF EXISTS {}",
                        crate::catalog::quote_ident(&s.name)
                    ),
                    None => continue,
                },
                _ => continue,
            };
            stmts.push((
                *oid,
                format!("{sql} CASCADE"),
                self.describe(*kind, *oid, 0),
            ));
        }
        // Operators and extended statistics.
        let sp = self.search_path().to_vec();
        let mut others = Vec::new();
        for o in self
            .cat
            .operators
            .values()
            .filter(|o| roles.contains(&o.owner))
        {
            let ty = |t: Option<&DataType>| {
                t.map_or("NONE".to_string(), |t| self.cat.format_type(t, None, &sp))
            };
            let sql = format!(
                "DROP OPERATOR IF EXISTS {}.{}({}, {}) CASCADE",
                crate::catalog::quote_ident(self.cat.schema_name(o.schema)),
                o.name,
                ty(o.left.as_ref()),
                ty(Some(&o.right))
            );
            others.push((
                o.oid,
                sql,
                crate::catalog::depend::describe(
                    &self.cat,
                    &sp,
                    (crate::catalog::depend::OPERATOR, o.oid, 0),
                )
                .unwrap_or_default(),
            ));
        }
        for x in self
            .cat
            .statistics
            .values()
            .filter(|x| roles.contains(&x.owner))
        {
            let sql = format!(
                "DROP STATISTICS IF EXISTS {}.{}",
                crate::catalog::quote_ident(self.cat.schema_name(x.schema)),
                crate::catalog::quote_ident(&x.name)
            );
            others.push((
                x.oid,
                sql,
                crate::catalog::depend::describe(
                    &self.cat,
                    &sp,
                    (crate::catalog::depend::STATEXT, x.oid, 0),
                )
                .unwrap_or_default(),
            ));
        }
        // Foreign servers, wrappers and the roles' user mappings.
        for x in self.cat.servers.values().filter(|x| roles.contains(&x.owner)) {
            others.push((
                x.oid,
                format!(
                    "DROP SERVER IF EXISTS {} CASCADE",
                    crate::catalog::quote_ident(&x.name)
                ),
                format!("server {}", x.name),
            ));
        }
        for x in self.cat.fdws.values().filter(|x| roles.contains(&x.owner)) {
            others.push((
                x.oid,
                format!(
                    "DROP FOREIGN DATA WRAPPER IF EXISTS {} CASCADE",
                    crate::catalog::quote_ident(&x.name)
                ),
                format!("foreign-data wrapper {}", x.name),
            ));
        }
        for m in self
            .cat
            .user_mappings
            .values()
            .filter(|m| roles.contains(&m.user))
        {
            let Some(srv) = self.cat.servers.get(&m.server) else {
                continue;
            };
            let user = self.role_name(m.user);
            others.push((
                m.oid,
                format!(
                    "DROP USER MAPPING IF EXISTS FOR {} SERVER {}",
                    crate::catalog::quote_ident(&user),
                    crate::catalog::quote_ident(&srv.name)
                ),
                format!("user mapping for {user} on server {}", srv.name),
            ));
        }
        // Publications (tablespaces and subscriptions are shared objects: they stay).
        for p in self
            .cat
            .publications
            .values()
            .filter(|p| roles.contains(&p.owner))
        {
            others.push((
                p.oid,
                format!(
                    "DROP PUBLICATION IF EXISTS {}",
                    crate::catalog::quote_ident(&p.name)
                ),
                format!("publication {}", p.name),
            ));
        }
        // Newest first.
        stmts.extend(others.iter().cloned());
        stmts.sort_by_key(|a| std::cmp::Reverse(a.0));
        let mut mine: Vec<String> = owned
            .iter()
            .map(|(k, o, _)| self.describe(*k, *o, 0))
            .collect();
        mine.extend(owned_ts.iter().map(|(k, o, _)| self.ts_description(*k, *o)));
        mine.extend(others.into_iter().map(|o| o.2));
        self.save()?;
        let mut outside = Vec::new();
        for (_, sql, what) in stmts {
            let parsed = crate::sql::parse::parse(&sql)?;
            for st in parsed {
                if let Some(n) = st.stmt.as_deref().and_then(|n| n.node.as_ref()) {
                    self.reload_catalog();
                    let before = self.ctx.notices.borrow().len();
                    let hold = self.ctx.sess.hold_notices();
                    self.run(n)?;
                    drop(hold);
                    let notices: Vec<_> = self.ctx.notices.borrow_mut().drain(before..).collect();
                    for nt in notices {
                        if !nt.message.starts_with("drop cascades to ") {
                            continue;
                        }
                        // One dependent in the message, or several in the detail.
                        let deps: Vec<String> = match nt.message.ends_with(" other objects") {
                            true => nt
                                .detail
                                .iter()
                                .flat_map(|d| d.lines())
                                .filter_map(|l| l.strip_prefix("drop cascades to "))
                                .map(str::to_string)
                                .collect(),
                            false => nt
                                .message
                                .strip_prefix("drop cascades to ")
                                .map(str::to_string)
                                .into_iter()
                                .collect(),
                        };
                        // (A part of an object of theirs, a trigger on their table, say, goes
                        // with it. Another role's user mapping on their server is no part of
                        // it.)
                        let deps: Vec<String> = deps
                            .into_iter()
                            .filter(|d| {
                                !mine.iter().any(|m| {
                                    d == m
                                        || !d.starts_with("user mapping for ")
                                            && (d.ends_with(&format!(" on {m}"))
                                                || d.ends_with(&format!(" of {m}")))
                                })
                            })
                            .collect();
                        if deps.is_empty() {
                            continue;
                        }
                        if !cascade {
                            outside.extend(deps.iter().map(|d| format!("{d} depends on {what}")));
                        } else {
                            self.ctx.notice(match deps.len() {
                                1 => super::super::notice(format!("drop cascades to {}", deps[0])),
                                n => super::super::notice(format!(
                                    "drop cascades to {n} other objects"
                                ))
                                .detail(
                                    deps.iter()
                                        .map(|d| format!("drop cascades to {d}"))
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                ),
                            });
                        }
                    }
                }
            }
        }
        if !outside.is_empty() {
            // (reportDependentObjects: one object to drop is named.)
            return Err(Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                match mine.as_slice() {
                    [one] => format!("cannot drop {one} because other objects depend on it"),
                    _ => "cannot drop desired object(s) because other objects depend on them".into(),
                },
            )
            .detail(outside.join("\n"))
            .hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        Ok("DROP OWNED".into())
    }
}

/// What revoking a role grant does to each of the role's grants (RevokeRoleGrantAction).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rrg {
    Noop,
    Delete,
    RemoveAdmin,
    RemoveInherit,
    RemoveSet,
}

/// plan_recursive_revoke: revoke grant `index` (or only its ADMIN OPTION) and, unless its
/// member keeps ADMIN OPTION through another grant, the grants it made.
fn plan_recursive_revoke(
    list: &[AuthMember],
    actions: &mut [Rrg],
    index: usize,
    admin_only: bool,
    cascade: bool,
) -> Result<()> {
    if actions[index] == Rrg::Delete || (actions[index] == Rrg::RemoveAdmin && admin_only) {
        return Ok(());
    }
    let m = &list[index];
    if !admin_only {
        actions[index] = Rrg::Delete;
        if !m.admin {
            return Ok(());
        }
    } else {
        if !m.admin {
            return Ok(());
        }
        actions[index] = Rrg::RemoveAdmin;
    }
    if (0..list.len()).any(|i| list[i].member == m.member && list[i].admin && actions[i] == Rrg::Noop)
    {
        return Ok(());
    }
    for i in 0..list.len() {
        if list[i].grantor == m.member && actions[i] != Rrg::Delete {
            if !cascade {
                return Err(Error::new(
                    code::DEPENDENT_OBJECTS_STILL_EXIST,
                    "dependent privileges exist",
                )
                .hint("Use CASCADE to revoke them too."));
            }
            plan_recursive_revoke(list, actions, i, false, cascade)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct RoleLists {
    /// PASSWORD: the string given (None: PASSWORD NULL).
    password_raw: Option<Option<String>>,
    /// The RoleSpecs of IN ROLE / ROLE / ADMIN (looked up once the role exists).
    in_role: Vec<pb::Node>,
    role: Vec<pb::Node>,
    admin: Vec<pb::Node>,
    password: bool,
    seen: Vec<String>,
}

/// The privileges a GRANT's object type allows and the word its errors use.
fn grant_kind(t: pb::ObjectType) -> Result<(u32, &'static str)> {
    use pb::ObjectType as O;
    Ok(match t {
        O::ObjectTable => (ObjKind::Table.all() | ObjKind::Sequence.all(), "relation"),
        O::ObjectSequence => (ObjKind::Sequence.all(), "sequence"),
        O::ObjectDatabase => (ObjKind::Database.all(), "database"),
        O::ObjectDomain => (ObjKind::Type.all(), "domain"),
        O::ObjectFunction => (ObjKind::Function.all(), "function"),
        O::ObjectProcedure => (ObjKind::Function.all(), "procedure"),
        O::ObjectRoutine => (ObjKind::Function.all(), "routine"),
        O::ObjectLanguage => (ObjKind::Language.all(), "language"),
        O::ObjectLargeobject => (ObjKind::LargeObject.all(), "large object"),
        O::ObjectSchema => (ObjKind::Schema.all(), "schema"),
        O::ObjectTablespace => (ObjKind::Tablespace.all(), "tablespace"),
        O::ObjectType => (ObjKind::Type.all(), "type"),
        O::ObjectFdw => (ObjKind::ForeignDataWrapper.all(), "foreign-data wrapper"),
        O::ObjectForeignServer => (ObjKind::ForeignServer.all(), "foreign server"),
        O::ObjectParameterAcl => (ObjKind::Parameter.all(), "parameter"),
        _ => return Err(crate::error::unsupported("GRANT on this kind of object")),
    })
}

/// Two ACLs with the same items (in any order).
fn same_acl(a: &Acl, b: &Acl) -> bool {
    a.len() == b.len() && a.iter().all(|i| b.contains(i))
}

/// aclnewowner: the old owner's entries (as grantee or grantor) become the new owner's,
/// merged with any the new owner already had.
pub fn new_owner_acl(old: &Acl, from: u32, to: u32) -> Acl {
    let mut out: Acl = Vec::new();
    for item in old {
        let mut i: AclItem = *item;
        if i.grantee == from {
            i.grantee = to;
        }
        if i.grantor == from {
            i.grantor = to;
        }
        match out
            .iter_mut()
            .find(|o| o.grantee == i.grantee && o.grantor == i.grantor)
        {
            Some(o) => {
                o.privs |= i.privs;
                o.goptions |= i.goptions;
            }
            None => out.push(i),
        }
    }
    out
}

/// RemoveRoleFromObjectACL: REVOKE ALL FROM `roles` CASCADE on one ACL. Their entries go,
/// and so (recursively) do the grants that rested on their grant options; the owner's own
/// grants stay.
fn revoke_roles_cascade(acl: &mut Acl, roles: &[u32], owner: Option<u32>) {
    acl.retain(|i| !roles.contains(&i.grantee));
    let mut dead: Vec<u32> = roles.to_vec();
    loop {
        let gone = |i: &crate::catalog::acl::AclItem| {
            dead.contains(&i.grantor) && Some(i.grantor) != owner
        };
        let removed: Vec<u32> = acl.iter().filter(|i| gone(i)).map(|i| i.grantee).collect();
        if removed.is_empty() {
            break;
        }
        acl.retain(|i| !gone(i));
        for g in removed {
            if !dead.contains(&g) && !acl.iter().any(|i| i.grantee == g && i.goptions != 0) {
                dead.push(g);
            }
        }
    }
}
