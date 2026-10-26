//! Foreign-data wrappers, foreign servers and user mappings (foreigncmds.c): CREATE /
//! ALTER / DROP, their generic options (transformGenericOptions) and the libpq option check
//! of postgresql_fdw_validator.

use super::Ddl;
use crate::catalog::userobj::{ForeignDataWrapper, ForeignServer, UserMapping};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{def_arg_str, inner, names, with_pos};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// postgresql_fdw_validator.
const POSTGRESQL_FDW_VALIDATOR: u32 = 2316;
const FDW_HANDLER: u32 = 3115;
const TEXT_ARRAY: u32 = 1009;
const OID: u32 = 26;

/// libpq_conninfo_options: the options postgresql_fdw_validator accepts, by catalog.
const LIBPQ_OPTIONS: &[(&str, u32)] = &[
    ("authtype", 1417),
    ("service", 1417),
    ("user", 1418),
    ("password", 1418),
    ("connect_timeout", 1417),
    ("dbname", 1417),
    ("host", 1417),
    ("hostaddr", 1417),
    ("port", 1417),
    ("tty", 1417),
    ("options", 1417),
    ("requiressl", 1417),
    ("sslmode", 1417),
    ("gsslib", 1417),
    ("gssdelegation", 1417),
];

/// varstr_levenshtein (insertion, deletion and substitution cost 1).
fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// getClosestMatch over `candidates` (varlena.c's ClosestMatchState with max_d 4).
pub(crate) fn closest_match<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    if name.is_empty() {
        return None;
    }
    for n in candidates {
        let d = levenshtein(name, n);
        if d <= 4 && d <= name.len() / 2 && best.is_none_or(|b| d < b.0) {
            best = Some((d, n));
        }
    }
    best.map(|b| b.1)
}

/// postgresql_fdw_validator: every option must be a libpq connection option of the
/// catalog's context.
fn postgresql_validate(options: &[String], catalog: u32) -> Result<()> {
    for o in options {
        let name = o.split_once('=').map_or(o.as_str(), |(n, _)| n);
        if LIBPQ_OPTIONS.iter().any(|(n, c)| *n == name && *c == catalog) {
            continue;
        }
        let mut best: Option<(usize, &str)> = None;
        let mut any = false;
        for (n, c) in LIBPQ_OPTIONS {
            if *c != catalog {
                continue;
            }
            any = true;
            let d = levenshtein(name, n);
            if d <= 4 && d <= name.len() / 2 && best.is_none_or(|b| d < b.0) {
                best = Some((d, n));
            }
        }
        let e = Error::new(code::SYNTAX_ERROR, format!("invalid option \"{name}\""));
        return Err(match (any, best) {
            (true, Some((_, m))) => e.hint(format!("Perhaps you meant the option \"{m}\".")),
            (true, None) => e,
            (false, _) => e.hint("There are no valid options in this context."),
        });
    }
    Ok(())
}

impl Ddl<'_, '_> {
    /// transformGenericOptions: OPTIONS (ADD / SET / DROP ...) applied to `old`, then the
    /// validator's check.
    pub(super) fn generic_options(
        &self,
        catalog: u32,
        old: Option<&Vec<String>>,
        opts: &[pb::Node],
        validator: u32,
    ) -> Result<Option<Vec<String>>> {
        let mut list: Vec<(String, String)> = old
            .map(|v| {
                v.iter()
                    .map(|o| {
                        let (n, v) = o.split_once('=').unwrap_or((o.as_str(), ""));
                        (n.to_string(), v.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        for d in super::def_elems(opts) {
            let pos = list.iter().position(|(n, _)| *n == d.defname);
            let value = def_arg_str(d).unwrap_or_default();
            match pb::DefElemAction::try_from(d.defaction).unwrap_or(pb::DefElemAction::DefelemUnspec) {
                pb::DefElemAction::DefelemDrop => match pos {
                    Some(i) => {
                        list.remove(i);
                    }
                    None => {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("option \"{}\" not found", d.defname),
                        ));
                    }
                },
                pb::DefElemAction::DefelemSet => match pos {
                    Some(i) => list[i] = (d.defname.clone(), value),
                    None => {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("option \"{}\" not found", d.defname),
                        ));
                    }
                },
                _ => {
                    if pos.is_some() {
                        return Err(Error::new(
                            code::DUPLICATE_OBJECT,
                            format!("option \"{}\" provided more than once", d.defname),
                        ));
                    }
                    list.push((d.defname.clone(), value));
                }
            }
        }
        let result: Vec<String> = list.into_iter().map(|(n, v)| format!("{n}={v}")).collect();
        if validator == POSTGRESQL_FDW_VALIDATOR {
            postgresql_validate(&result, catalog)?;
        } else if let Some(f) = self.cat.functions.get(&validator)
            && let Some(bin) = &f.probin
            && let Some(v) = crate::ext::native::fdw_validator(bin, &f.body)
        {
            v(&crate::ext::native::FdwValidate {
                options: &result,
                catalog,
                cat: &self.cat,
                superuser: self.is_superuser(),
                notice: &|e| self.ctx.notice(e),
            })?;
        }
        Ok((!result.is_empty()).then_some(result))
    }

    pub(super) fn fdw_named(&self, name: &str) -> Result<Arc<ForeignDataWrapper>> {
        self.cat
            .fdws
            .values()
            .find(|f| f.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("foreign-data wrapper \"{name}\" does not exist"),
                )
            })
    }

    pub(super) fn server_named(&self, name: &str) -> Result<Arc<ForeignServer>> {
        self.cat
            .servers
            .values()
            .find(|f| f.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("server \"{name}\" does not exist"),
                )
            })
    }

    /// parse_func_options: HANDLER / VALIDATOR (NO HANDLER / NO VALIDATOR: 0).
    fn fdw_func_options(&self, list: &[pb::Node]) -> Result<(Option<u32>, Option<u32>)> {
        let (mut handler, mut validator) = (None, None);
        for d in super::def_elems(list) {
            let parts = match d.arg.as_deref().and_then(inner) {
                Some(NodeEnum::List(l)) => Some(names(&l.items)),
                _ => None,
            };
            match d.defname.as_str() {
                "handler" => {
                    if handler.is_some() {
                        return Err(with_pos(
                            Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                            d.location,
                        ));
                    }
                    handler = Some(match parts {
                        None => 0,
                        Some(p) => {
                            let f = self.func_by_types(&p, &[])?;
                            if f.ret != FDW_HANDLER {
                                return Err(Error::new(
                                    code::WRONG_OBJECT_TYPE,
                                    format!("function {} must return type fdw_handler", p.join(".")),
                                ));
                            }
                            f.oid
                        }
                    });
                }
                "validator" => {
                    if validator.is_some() {
                        return Err(with_pos(
                            Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                            d.location,
                        ));
                    }
                    validator = Some(match parts {
                        None => 0,
                        Some(p) => self.func_by_types(&p, &[TEXT_ARRAY, OID])?.oid,
                    });
                }
                other => {
                    return Err(Error::new(
                        code::INTERNAL_ERROR,
                        format!("option \"{other}\" not recognized"),
                    ));
                }
            }
        }
        Ok((handler, validator))
    }

    /// CREATE FOREIGN DATA WRAPPER.
    pub(super) fn create_fdw(&mut self, s: &pb::CreateFdwStmt) -> Result<String> {
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create foreign-data wrapper \"{}\"", s.fdwname),
            )
            .hint("Must be superuser to create a foreign-data wrapper."));
        }
        if self.cat.fdws.values().any(|f| f.name == s.fdwname) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("foreign-data wrapper \"{}\" already exists", s.fdwname),
            ));
        }
        let (handler, validator) = self.fdw_func_options(&s.func_options)?;
        let validator = validator.unwrap_or(0);
        let options = self.generic_options(2328, None, &s.options, validator)?;
        let oid = self.cat.alloc_oid();
        self.cat.fdws.insert(
            oid,
            Arc::new(ForeignDataWrapper {
                oid,
                name: s.fdwname.clone(),
                owner: self.me(),
                handler: handler.unwrap_or(0),
                validator,
                options,
                heap_order: 0,
            }),
        );
        self.save()?;
        Ok("CREATE FOREIGN DATA WRAPPER".into())
    }

    /// ALTER FOREIGN DATA WRAPPER ... HANDLER / VALIDATOR / OPTIONS.
    pub(super) fn alter_fdw(&mut self, s: &pb::AlterFdwStmt) -> Result<String> {
        let f = self.fdw_named(&s.fdwname)?;
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to alter foreign-data wrapper \"{}\"", s.fdwname),
            )
            .hint("Must be superuser to alter a foreign-data wrapper."));
        }
        let (handler, validator) = self.fdw_func_options(&s.func_options)?;
        let mut nf = (*f).clone();
        if let Some(h) = handler {
            nf.handler = h;
            self.ctx.notice(crate::session::warning(
                code::WARNING,
                "changing the foreign-data wrapper handler can change behavior of existing foreign tables",
            ));
        }
        if let Some(v) = validator {
            nf.validator = v;
            if v != 0 {
                self.ctx.notice(crate::session::warning(
                    code::WARNING,
                    "changing the foreign-data wrapper validator can cause the options for dependent objects to become invalid",
                ));
            }
        }
        if !s.options.is_empty() {
            nf.options = self.generic_options(2328, f.options.as_ref(), &s.options, nf.validator)?;
        }
        self.cat.fdws.insert(nf.oid, Arc::new(nf));
        self.save()?;
        Ok("ALTER FOREIGN DATA WRAPPER".into())
    }

    /// CREATE SERVER.
    pub(super) fn create_server(&mut self, s: &pb::CreateForeignServerStmt) -> Result<String> {
        const TAG: &str = "CREATE SERVER";
        if self.cat.servers.values().any(|x| x.name == s.servername) {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_OBJECT,
                    format!("server \"{}\" already exists, skipping", s.servername),
                );
                return Ok(TAG.into());
            }
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("server \"{}\" already exists", s.servername),
            ));
        }
        let f = self.fdw_named(&s.fdwname)?;
        let me = self.me();
        let (p, _) = self.cat.obj_mask(
            crate::catalog::acl::ObjKind::ForeignDataWrapper,
            f.oid,
            me,
            self.db_owner(),
        );
        if p & crate::catalog::acl::USAGE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for foreign-data wrapper {}", f.name),
            ));
        }
        let options = self.generic_options(1417, None, &s.options, f.validator)?;
        let oid = self.cat.alloc_oid();
        self.cat.servers.insert(
            oid,
            Arc::new(ForeignServer {
                oid,
                name: s.servername.clone(),
                owner: me,
                fdw: f.oid,
                kind: (!s.servertype.is_empty()).then(|| s.servertype.clone()),
                version: (!s.version.is_empty()).then(|| s.version.clone()),
                options,
                heap_order: 0,
            }),
        );
        self.save()?;
        Ok(TAG.into())
    }

    /// ALTER SERVER ... VERSION / OPTIONS.
    pub(super) fn alter_server(&mut self, s: &pb::AlterForeignServerStmt) -> Result<String> {
        let srv = self.server_named(&s.servername)?;
        if !self.owns_role(srv.owner) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of foreign server {}", s.servername),
            ));
        }
        let mut n = (*srv).clone();
        if s.has_version {
            n.version = (!s.version.is_empty()).then(|| s.version.clone());
        }
        if !s.options.is_empty() {
            let validator = self.cat.fdws.get(&srv.fdw).map_or(0, |f| f.validator);
            n.options = self.generic_options(1417, srv.options.as_ref(), &s.options, validator)?;
        }
        self.cat.servers.insert(n.oid, Arc::new(n));
        self.save()?;
        Ok("ALTER SERVER".into())
    }

    fn owns_role(&self, owner: u32) -> bool {
        self.cat.has_privs_of_role(self.me(), owner, self.db_owner())
    }

    /// The role a user mapping names (0: PUBLIC); None for a missing role with
    /// `missing_ok`.
    fn mapping_user(&self, r: Option<&pb::RoleSpec>, missing_ok: bool) -> Result<Option<u32>> {
        let Some(r) = r else { return Ok(Some(0)) };
        if r.roletype == pb::RoleSpecType::RolespecPublic as i32 {
            return Ok(Some(0));
        }
        match self.role_oid(r) {
            Ok(o) => Ok(Some(o)),
            Err(_) if missing_ok && r.roletype == pb::RoleSpecType::RolespecCstring as i32 => {
                self.notice(format!("role \"{}\" does not exist, skipping", r.rolename));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn mapping_user_name(&self, user: u32) -> String {
        if user == 0 {
            "public".into()
        } else {
            self.role_name(user)
        }
    }

    /// user_mapping_ddl_aclcheck.
    fn mapping_acl(&self, user: u32, srv: &ForeignServer) -> Result<()> {
        if self.owns_role(srv.owner) {
            return Ok(());
        }
        let me = self.me();
        if user == me {
            let (p, _) = self.cat.obj_mask(
                crate::catalog::acl::ObjKind::ForeignServer,
                srv.oid,
                me,
                self.db_owner(),
            );
            if p & crate::catalog::acl::USAGE != 0 {
                return Ok(());
            }
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for foreign server {}", srv.name),
            ));
        }
        Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of foreign server {}", srv.name),
        ))
    }

    /// CREATE USER MAPPING.
    pub(super) fn create_user_mapping(&mut self, s: &pb::CreateUserMappingStmt) -> Result<String> {
        const TAG: &str = "CREATE USER MAPPING";
        let user = self.mapping_user(s.user.as_ref(), false)?.unwrap_or(0);
        let srv = self.server_named(&s.servername)?;
        self.mapping_acl(user, &srv)?;
        if self
            .cat
            .user_mappings
            .values()
            .any(|m| m.user == user && m.server == srv.oid)
        {
            let msg = format!(
                "user mapping for \"{}\" already exists for server \"{}\"",
                self.mapping_user_name(user),
                s.servername
            );
            if s.if_not_exists {
                self.notice_code(code::DUPLICATE_OBJECT, format!("{msg}, skipping"));
                return Ok(TAG.into());
            }
            return Err(Error::new(code::DUPLICATE_OBJECT, msg));
        }
        let validator = self.cat.fdws.get(&srv.fdw).map_or(0, |f| f.validator);
        let options = self.generic_options(1418, None, &s.options, validator)?;
        let oid = self.cat.alloc_oid();
        self.cat.user_mappings.insert(
            oid,
            Arc::new(UserMapping {
                oid,
                user,
                server: srv.oid,
                options,
            }),
        );
        self.save()?;
        Ok(TAG.into())
    }

    /// ALTER USER MAPPING ... OPTIONS.
    pub(super) fn alter_user_mapping(&mut self, s: &pb::AlterUserMappingStmt) -> Result<String> {
        let user = self.mapping_user(s.user.as_ref(), false)?.unwrap_or(0);
        let srv = self.server_named(&s.servername)?;
        let Some(m) = self
            .cat
            .user_mappings
            .values()
            .find(|m| m.user == user && m.server == srv.oid)
            .cloned()
        else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "user mapping for \"{}\" does not exist for server \"{}\"",
                    self.mapping_user_name(user),
                    s.servername
                ),
            ));
        };
        self.mapping_acl(user, &srv)?;
        if !s.options.is_empty() {
            let validator = self.cat.fdws.get(&srv.fdw).map_or(0, |f| f.validator);
            let mut n = (*m).clone();
            n.options = self.generic_options(1418, m.options.as_ref(), &s.options, validator)?;
            self.cat.user_mappings.insert(n.oid, Arc::new(n));
        }
        self.save()?;
        Ok("ALTER USER MAPPING".into())
    }

    /// DROP USER MAPPING (RemoveUserMapping).
    pub(super) fn drop_user_mapping(&mut self, s: &pb::DropUserMappingStmt) -> Result<String> {
        const TAG: &str = "DROP USER MAPPING";
        let Some(user) = self.mapping_user(s.user.as_ref(), s.missing_ok)? else {
            return Ok(TAG.into());
        };
        let Some(srv) = self.cat.servers.values().find(|x| x.name == s.servername).cloned() else {
            if !s.missing_ok {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("server \"{}\" does not exist", s.servername),
                ));
            }
            self.notice(format!("server \"{}\" does not exist, skipping", s.servername));
            return Ok(TAG.into());
        };
        let Some(m) = self
            .cat
            .user_mappings
            .values()
            .find(|m| m.user == user && m.server == srv.oid)
            .cloned()
        else {
            let msg = format!(
                "user mapping for \"{}\" does not exist for server \"{}\"",
                self.mapping_user_name(user),
                s.servername
            );
            if !s.missing_ok {
                return Err(Error::new(code::UNDEFINED_OBJECT, msg));
            }
            self.notice(format!("{msg}, skipping"));
            return Ok(TAG.into());
        };
        self.mapping_acl(user, &srv)?;
        self.drop_objects(&[(crate::catalog::depend::USER_MAPPING, m.oid, 0)], true)?;
        self.save()?;
        Ok(TAG.into())
    }

    /// The foreign-data wrapper or server a DROP names; None when missing with IF EXISTS.
    pub(super) fn fdw_to_drop(&self, server: bool, name: &str, missing_ok: bool) -> Result<Option<u32>> {
        let found = if server {
            self.cat
                .servers
                .values()
                .find(|x| x.name == name)
                .map(|x| (x.oid, x.owner))
        } else {
            self.cat
                .fdws
                .values()
                .find(|x| x.name == name)
                .map(|x| (x.oid, x.owner))
        };
        let what = if server { "server" } else { "foreign-data wrapper" };
        match found {
            Some((oid, owner)) => {
                if !self.owns_role(owner) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!(
                            "must be owner of {} {name}",
                            if server { "foreign server" } else { "foreign-data wrapper" }
                        ),
                    ));
                }
                Ok(Some(oid))
            }
            None if missing_ok => {
                self.notice(format!("{what} \"{name}\" does not exist, skipping"));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("{what} \"{name}\" does not exist"),
            )),
        }
    }

    /// ALTER FOREIGN DATA WRAPPER / SERVER ... RENAME TO.
    pub(super) fn rename_fdw(&mut self, server: bool, s: &pb::RenameStmt) -> Result<String> {
        let name = s
            .object
            .as_deref()
            .and_then(crate::sql::binder::ast::sval)
            .map(str::to_string)
            .unwrap_or_else(|| s.subname.clone());
        let newname = crate::catalog::truncate_ident(&s.newname);
        if server {
            let x = self.server_named(&name)?;
            if !self.is_superuser() && !self.owns_role(x.owner) {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("must be owner of foreign server {name}"),
                ));
            }
            if self.cat.servers.values().any(|y| y.name == newname) {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("server \"{newname}\" already exists"),
                ));
            }
            let mut n = (*x).clone();
            n.name = newname;
            self.cat.servers.insert(n.oid, Arc::new(n));
            self.save()?;
            return Ok("ALTER SERVER".into());
        }
        let x = self.fdw_named(&name)?;
        if !self.is_superuser() && !self.owns_role(x.owner) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of foreign-data wrapper {name}"),
            ));
        }
        if self.cat.fdws.values().any(|y| y.name == newname) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("foreign-data wrapper \"{newname}\" already exists"),
            ));
        }
        let mut n = (*x).clone();
        n.name = newname;
        self.cat.fdws.insert(n.oid, Arc::new(n));
        self.save()?;
        Ok("ALTER FOREIGN DATA WRAPPER".into())
    }

    /// ALTER FOREIGN DATA WRAPPER / SERVER ... OWNER TO.
    pub(super) fn owner_fdw(&mut self, server: bool, obj: Option<&pb::Node>, new: u32) -> Result<()> {
        let name = obj
            .and_then(crate::sql::binder::ast::sval)
            .unwrap_or_default()
            .to_string();
        if server {
            let x = self.server_named(&name)?;
            if x.owner == new {
                return Ok(());
            }
            if !self.is_superuser() {
                if !self.owns_role(x.owner) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("must be owner of foreign server {name}"),
                    ));
                }
                self.check_new_owner(new, None)?;
                let (p, _) = self.cat.obj_mask(
                    crate::catalog::acl::ObjKind::ForeignDataWrapper,
                    x.fdw,
                    new,
                    self.db_owner(),
                );
                if p & crate::catalog::acl::USAGE == 0 {
                    let fname = self.cat.fdws.get(&x.fdw).map(|f| f.name.clone()).unwrap_or_default();
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for foreign-data wrapper {fname}"),
                    ));
                }
            }
            self.reown_acls(crate::catalog::acl::ObjKind::ForeignServer, x.oid, x.owner, new);
            let mut n = (*x).clone();
            n.owner = new;
            self.cat.servers.insert(n.oid, Arc::new(n));
            return Ok(());
        }
        let x = self.fdw_named(&name)?;
        if x.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to change owner of foreign-data wrapper \"{name}\""),
            )
            .hint("Must be superuser to change owner of a foreign-data wrapper."));
        }
        if !self.cat.is_superuser(new) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to change owner of foreign-data wrapper \"{name}\""),
            )
            .hint("The owner of a foreign-data wrapper must be a superuser."));
        }
        self.reown_acls(crate::catalog::acl::ObjKind::ForeignDataWrapper, x.oid, x.owner, new);
        let mut n = (*x).clone();
        n.owner = new;
        self.cat.fdws.insert(n.oid, Arc::new(n));
        Ok(())
    }
}
