//! Access control: permissions, roles, the legacy user statements, identities (Cassandra 5) and
//! ScyllaDB's service levels. Ports `grantPermissionsStatement` through `dropIdentityStatement`
//! of Cassandra's `Parser.g`, with the checks its actions make (`filterPermissions`,
//! `canonicalizeObjectName`, `RoleOptions.setOption`).

use std::borrow::Cow;

use super::dfa_tables as dfa;
use super::{F_END, F_IDENT, PResult, Parser};
use crate::Dialect;
use crate::ast::*;
use crate::tokens::{self as t, Tt};

fn permission_of(ty: Tt, d: Dialect) -> Option<Permission> {
    Some(match ty {
        t::K_CREATE => Permission::Create,
        t::K_ALTER => Permission::Alter,
        t::K_DROP => Permission::Drop,
        t::K_SELECT => Permission::Select,
        t::K_MODIFY => Permission::Modify,
        t::K_AUTHORIZE => Permission::Authorize,
        t::K_DESCRIBE => Permission::Describe,
        t::K_EXECUTE => Permission::Execute,
        t::K_UNMASK if d == Dialect::Cassandra => Permission::Unmask,
        t::K_SELECT_MASKED if d == Dialect::Cassandra => Permission::SelectMasked,
        _ => return None,
    })
}

impl<'a> Parser<'a> {
    /// `permissionOrAll`. ScyllaDB takes one permission, not a list.
    fn permission_or_all(&mut self) -> PResult<Permissions> {
        let ty = self.la(1);
        if ty == t::K_ALL {
            self.consume();
            let keyword = self.eat(t::K_PERMISSIONS);
            return Ok(Permissions::All { keyword });
        }
        if permission_of(ty, self.d).is_none() {
            return self.no_viable(0);
        }
        let mut list = Vec::new();
        loop {
            let ty = self.la(1);
            let Some(p) = permission_of(ty, self.d) else {
                let first = list.is_empty();
                let r: PResult<Permissions> = self.mismatched_set();
                if !first {
                    // `$perms.add(null)` on the EnumSet.
                    return Err(self.null_pointer("Cannot invoke \"Object.getClass()\" because \"e\" is null"));
                }
                return r;
            };
            self.consume();
            let kw = self.eat(t::K_PERMISSION);
            list.push((p, kw));
            if self.scylla() || !self.eat(t::COMMA) {
                break;
            }
        }
        Ok(Permissions::List(list))
    }

    /// `resource`.
    fn resource(&mut self) -> PResult<Resource<'a>> {
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA167)?
        } else {
            match self.la(1) {
                t::K_ALL => match self.la(2) {
                    t::K_ROLES => 2,
                    t::K_FUNCTIONS => 3,
                    _ => 1,
                },
                t::K_ROLE => match self.la(2) {
                    t::K_TO | t::K_FROM | t::K_OF | t::EOF | t::K_NORECURSIVE | t::SEMI => 1,
                    ty if t::is_ident(ty, self.d) || matches!(ty, t::STRING_LITERAL | t::QMARK) => 2,
                    _ => return self.no_viable(0),
                },
                t::K_FUNCTION => match self.la(2) {
                    t::K_TO | t::K_FROM | t::K_OF | t::EOF | t::K_NORECURSIVE | t::SEMI => 1,
                    ty if t::is_ident(ty, self.d) || ty == t::QMARK => 3,
                    _ => return self.no_viable(0),
                },
                _ => 1,
            }
        };
        match alt {
            1 => self.data_resource(),
            2 => {
                if self.eat(t::K_ROLE) {
                    return Ok(Resource::Role(self.role_name()?));
                }
                self.expect(t::K_ALL)?;
                self.expect(t::K_ROLES)?;
                Ok(Resource::AllRoles)
            }
            3 => self.function_resource(),
            _ => self.jmx_resource(),
        }
    }

    fn data_resource(&mut self) -> PResult<Resource<'a>> {
        let ty = self.la(1);
        let alt = match ty {
            t::K_ALL => match self.la(2) {
                t::K_KEYSPACES => 1,
                t::K_TABLES if self.cassandra() => 4,
                t::EOF | t::K_FROM | t::K_NORECURSIVE | t::K_OF | t::K_TO | t::DOT | t::SEMI => 3,
                _ => return self.no_viable(1),
            },
            t::K_KEYSPACE => 2,
            t::K_COLUMNFAMILY | t::QMARK => 3,
            _ if t::is_ident(ty, self.d) => 3,
            _ => return self.no_viable(0),
        };
        match alt {
            1 => {
                self.consume();
                self.consume();
                Ok(Resource::AllKeyspaces)
            }
            2 => {
                self.consume();
                Ok(Resource::Keyspace(self.keyspace_name()?))
            }
            3 => {
                let keyword = self.eat(t::K_COLUMNFAMILY);
                let name = self.column_family_name()?;
                Ok(Resource::Table { name, keyword })
            }
            _ => {
                self.consume();
                self.consume();
                self.expect(t::K_IN)?;
                self.expect(t::K_KEYSPACE)?;
                Ok(Resource::AllTablesInKeyspace(self.keyspace_name()?))
            }
        }
    }

    fn function_resource(&mut self) -> PResult<Resource<'a>> {
        match self.la(1) {
            t::K_ALL => {
                if self.la(2) != t::K_FUNCTIONS {
                    return self.no_viable(1);
                }
                match self.la(3) {
                    t::K_IN => {
                        self.consume();
                        self.consume();
                        self.consume();
                        self.expect_f(t::K_KEYSPACE, &[F_IDENT, t::QMARK])?;
                        Ok(Resource::AllFunctionsInKeyspace(self.keyspace_name()?))
                    }
                    t::EOF | t::K_FROM | t::K_NORECURSIVE | t::K_OF | t::K_TO | t::SEMI => {
                        self.consume();
                        self.consume();
                        Ok(Resource::AllFunctions)
                    }
                    _ => self.no_viable(2),
                }
            }
            t::K_FUNCTION => {
                self.consume();
                let name = match self.function_name() {
                    Ok(n) => Some(n),
                    Err(f) if self.spec > 0 || self.scylla() => return Err(f),
                    Err(_) => None,
                };
                self.expect_f(t::LPAREN, &[F_IDENT, t::RPAREN])?;
                let mut arg_types = Vec::new();
                if super::ddl::is_type_start(self.la(1), self.d) {
                    arg_types.push(self.comparator_type()?);
                    while self.eat(t::COMMA) {
                        arg_types.push(self.comparator_type()?);
                    }
                }
                self.expect_f(t::RPAREN, &[F_END])?;
                let Some(name) = name else {
                    return Err(self.null_pointer("Cannot read field \"keyspace\" because \"fn\" is null"));
                };
                Ok(Resource::Function { name, arg_types })
            }
            _ => self.no_viable(0),
        }
    }

    fn jmx_resource(&mut self) -> PResult<Resource<'a>> {
        match self.la(1) {
            t::K_ALL => {
                self.consume();
                self.expect(t::K_MBEANS)?;
                Ok(Resource::AllMbeans)
            }
            t::K_MBEAN | t::K_MBEANS => {
                let plural = self.consume().ty == t::K_MBEANS;
                let tok = self.expect_f(t::STRING_LITERAL, &[F_END])?;
                let name = self.text(&tok);
                self.check_object_name(&name, plural)?;
                Ok(Resource::Mbean { name, plural })
            }
            _ => self.no_viable(0),
        }
    }

    /// `canonicalizeObjectName`: what `javax.management.ObjectName` rejects.
    fn check_object_name(&mut self, s: &str, plural: bool) -> PResult<()> {
        if s.is_empty() {
            self.custom_error("Empty JMX object name supplied")?;
        }
        if s == "*:*" {
            self.custom_error("Please use ALL MBEANS instead of wildcard pattern")?;
        }
        match crate::jmx::parse_object_name(s) {
            Some(pattern) => {
                if plural && !pattern {
                    self.custom_error(format!("Plural form used, but non-pattern JMX object name specified ({s})"))?;
                }
            }
            None => self.custom_error(format!("{s} is not a valid JMX object name"))?,
        }
        Ok(())
    }

    /// `filterPermissions`: some of the permissions must apply to the resource.
    fn filter_permissions(&mut self, perms: &Permissions, resource: &Resource<'a>) -> PResult<()> {
        if matches!(resource, Resource::Function { .. }) && self.cassandra() {
            // Cassandra resolves the function against the schema here; the engine does that.
            return Ok(());
        }
        let applicable = resource.applicable_permissions(self.d);
        if perms.set().iter().any(|p| applicable.contains(p)) {
            return Ok(());
        }
        let msg = match self.d {
            Dialect::Cassandra => {
                let class = match resource {
                    Resource::AllKeyspaces
                    | Resource::Keyspace(_)
                    | Resource::Table { .. }
                    | Resource::AllTablesInKeyspace(_) => "DataResource",
                    Resource::AllRoles | Resource::Role(_) => "RoleResource",
                    Resource::AllFunctions | Resource::AllFunctionsInKeyspace(_) | Resource::Function { .. } => {
                        "FunctionResource"
                    }
                    Resource::AllMbeans | Resource::Mbean { .. } => "JMXResource",
                };
                format!("Resource type {class} does not support any of the requested permissions")
            }
            Dialect::Scylla => {
                format!("Resource {} does not support any of the requested permissions.", scylla_resource_display(resource))
            }
        };
        self.custom_error(msg)
    }

    /// `GRANT permissions ON resource TO role`.
    pub(crate) fn grant_permissions(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_GRANT)?;
        let permissions = self.permission_or_all()?;
        self.expect_f(t::K_ON, &[F_IDENT, t::QMARK, t::K_KEYSPACE, t::K_COLUMNFAMILY, t::K_ROLE, t::K_FUNCTION, t::K_MBEAN, t::K_MBEANS])?;
        let resource = self.resource()?;
        self.expect_f(t::K_TO, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        let role = self.role_name()?;
        self.filter_permissions(&permissions, &resource)?;
        Ok(StatementKind::Grant(Box::new(PermissionsStatement { permissions, resource, role })))
    }

    /// `REVOKE permissions ON resource FROM role`.
    pub(crate) fn revoke_permissions(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_REVOKE)?;
        let permissions = self.permission_or_all()?;
        self.expect_f(t::K_ON, &[F_IDENT, t::QMARK, t::K_KEYSPACE, t::K_COLUMNFAMILY, t::K_ROLE, t::K_FUNCTION, t::K_MBEAN, t::K_MBEANS])?;
        let resource = self.resource()?;
        self.expect_f(t::K_FROM, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        let role = self.role_name()?;
        self.filter_permissions(&permissions, &resource)?;
        Ok(StatementKind::Revoke(Box::new(PermissionsStatement { permissions, resource, role })))
    }

    /// `LIST permissions [ON resource] [OF role] [NORECURSIVE]`.
    pub(crate) fn list_permissions(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_LIST)?;
        let permissions = self.permission_or_all()?;
        let resource = if self.eat(t::K_ON) { Some(self.resource()?) } else { None };
        let of = if self.eat(t::K_OF) { Some(self.role_name()?) } else { None };
        let recursive = !self.eat(t::K_NORECURSIVE);
        Ok(StatementKind::ListPermissions(Box::new(ListPermissions { permissions, resource, of, recursive })))
    }

    pub(crate) fn grant_role(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_GRANT)?;
        let role = self.role_name()?;
        self.expect_f(t::K_TO, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        let grantee = self.role_name()?;
        Ok(StatementKind::GrantRole(Box::new(RoleGrant { role, grantee })))
    }

    pub(crate) fn revoke_role(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_REVOKE)?;
        let role = self.role_name()?;
        self.expect_f(t::K_FROM, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        let grantee = self.role_name()?;
        Ok(StatementKind::RevokeRole(Box::new(RoleGrant { role, grantee })))
    }

    // -----------------------------------------------------------------------------------------
    // Roles
    // -----------------------------------------------------------------------------------------

    pub(crate) fn create_role(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_ROLE)?;
        let if_exists = self.ddl_if_not_exists()?;
        let name = self.role_name()?;
        let (with, options) = self.role_options()?;
        Ok(StatementKind::CreateRole(Box::new(RoleStatement { if_exists, name, with, options })))
    }

    pub(crate) fn alter_role(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.expect(t::K_ROLE)?;
        let if_exists = if self.cassandra() { self.ddl_if_exists()? } else { false };
        let name = self.role_name()?;
        let (with, options) = self.role_options()?;
        Ok(StatementKind::AlterRole(Box::new(RoleStatement { if_exists, name, with, options })))
    }

    pub(crate) fn drop_role(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_ROLE)?;
        let if_exists = self.ddl_if_exists()?;
        let name = self.role_name()?;
        Ok(StatementKind::DropRole(Box::new(DropRole { if_exists, name })))
    }

    pub(crate) fn list_roles(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_LIST)?;
        self.expect(t::K_ROLES)?;
        let of = if self.eat(t::K_OF) { Some(self.role_name()?) } else { None };
        let recursive = !self.eat(t::K_NORECURSIVE);
        Ok(StatementKind::ListRoles(Box::new(ListRoles { of, recursive })))
    }

    fn ddl_if_not_exists(&mut self) -> PResult<bool> {
        if !self.at(t::K_IF) {
            return Ok(false);
        }
        self.consume();
        self.expect_f(t::K_NOT, &[t::K_EXISTS])?;
        self.expect_f(t::K_EXISTS, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        Ok(true)
    }

    fn ddl_if_exists(&mut self) -> PResult<bool> {
        if !self.at(t::K_IF) {
            return Ok(false);
        }
        self.consume();
        self.expect_f(t::K_EXISTS, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        Ok(true)
    }

    /// `[WITH roleOptions]`.
    fn role_options(&mut self) -> PResult<(bool, Vec<RoleOption<'a>>)> {
        if !self.eat(t::K_WITH) {
            return Ok((false, Vec::new()));
        }
        let mut options: Vec<RoleOption<'a>> = Vec::new();
        let mut dcs = BuilderState::default();
        let mut cidrs = BuilderState::default();
        loop {
            let o = self.role_option(&mut dcs, &mut cidrs)?;
            if let Some(key) = option_key(&o)
                && options.iter().any(|x| option_key(x) == Some(key))
                && self.cassandra()
            {
                return Err(self.throw_syntax(format!("Multiple definition for property '{key}'")));
            }
            options.push(o);
            if !self.eat(t::K_AND) {
                break;
            }
        }
        let has_pw = options.iter().any(|o| matches!(o, RoleOption::Password(_)));
        let has_hashed = options.iter().any(|o| matches!(o, RoleOption::HashedPassword(_)));
        if has_pw && has_hashed && self.cassandra() {
            return Err(self.throw_syntax("Options 'password' and 'hashed password' are mutually exclusive".into()));
        }
        Ok((true, options))
    }

    /// A `SyntaxException` thrown by a parser action.
    fn throw_syntax(&mut self, msg: String) -> super::Fail {
        self.throw(crate::SyntaxError::custom(msg))
    }

    fn role_option(&mut self, dcs: &mut BuilderState, cidrs: &mut BuilderState) -> PResult<RoleOption<'a>> {
        let alt = if self.cassandra() {
            self.predict(&dfa::DFA194)?
        } else {
            match self.la(1) {
                t::K_PASSWORD => 1,
                t::K_HASHED => 2,
                t::K_OPTIONS => 3,
                t::K_SUPERUSER => 4,
                t::K_LOGIN => 5,
                _ => return self.no_viable(0),
            }
        };
        let string = |p: &mut Self| -> PResult<Cow<'a, str>> {
            let tok = p.expect_f(t::STRING_LITERAL, &[F_END, t::K_AND])?;
            Ok(p.text(&tok))
        };
        Ok(match alt {
            1 => {
                self.consume();
                self.expect_f(t::EQ, &[t::STRING_LITERAL])?;
                RoleOption::Password(string(self)?)
            }
            2 => {
                self.consume();
                self.expect(t::K_PASSWORD)?;
                self.expect_f(t::EQ, &[t::STRING_LITERAL])?;
                RoleOption::HashedPassword(string(self)?)
            }
            3 => {
                self.consume();
                self.expect_f(t::EQ, &[t::LBRACE])?;
                let map = self.role_options_map()?;
                RoleOption::Options(map)
            }
            4 | 5 => {
                self.consume();
                self.expect_f(t::EQ, &[t::BOOLEAN])?;
                let tok = self.expect_f(t::BOOLEAN, &[F_END, t::K_AND])?;
                let v = self.src[tok.start as usize..tok.end as usize].eq_ignore_ascii_case("true");
                if alt == 4 { RoleOption::Superuser(v) } else { RoleOption::Login(v) }
            }
            6 | 7 => {
                self.consume();
                self.expect(t::K_TO)?;
                if alt == 6 {
                    self.expect(t::K_ALL)?;
                    self.expect(t::K_DATACENTERS)?;
                    if !dcs.set_all() {
                        return Err(self.throw_runtime("IllegalArgumentException", "DCs have already been set"));
                    }
                    RoleOption::AccessToAllDatacenters
                } else {
                    self.expect(t::K_DATACENTERS)?;
                    let list = self.string_set(dcs)?;
                    RoleOption::AccessToDatacenters(list)
                }
            }
            _ => {
                self.consume();
                self.expect(t::K_FROM)?;
                if alt == 8 {
                    self.expect(t::K_ALL)?;
                    self.expect(t::K_CIDRS)?;
                    if !cidrs.set_all() {
                        return Err(self.throw_runtime("IllegalArgumentException", "CIDR Groups have already been set"));
                    }
                    RoleOption::AccessFromAllCidrs
                } else {
                    self.expect(t::K_CIDRS)?;
                    let list = self.string_set(cidrs)?;
                    RoleOption::AccessFromCidrs(list)
                }
            }
        })
    }

    /// `'{' STRING (',' STRING)* '}'` of `ACCESS TO DATACENTERS` / `ACCESS FROM CIDRS`.
    fn string_set(&mut self, state: &mut BuilderState) -> PResult<Vec<Cow<'a, str>>> {
        self.expect_f(t::LBRACE, &[t::STRING_LITERAL])?;
        let mut out = Vec::new();
        loop {
            let tok = self.expect(t::STRING_LITERAL)?;
            if !state.add() {
                return Err(self.throw_runtime("IllegalArgumentException", "All has been set"));
            }
            out.push(self.text(&tok));
            if !self.eat(t::COMMA) {
                break;
            }
        }
        self.expect_f(t::RBRACE, &[F_END])?;
        Ok(out)
    }

    /// The map of `OPTIONS = {...}`: constants only (`convertPropertyMap`).
    fn role_options_map(&mut self) -> PResult<Vec<(Literal<'a>, Literal<'a>)>> {
        // Reuse the WITH-property path: parse as a map property named `options`.
        self.expect(t::LBRACE)?;
        let mut entries = Vec::new();
        let ty = self.la(1);
        if super::is_term_start(ty, self.d) || (ty == t::MINUS && self.cassandra()) {
            loop {
                let k = self.term()?;
                self.expect_f(t::COLON, &[super::F_TERM])?;
                let v = self.term()?;
                entries.push((k, v));
                if !self.eat(t::COMMA) {
                    break;
                }
            }
        }
        self.expect_f(t::RBRACE, &[F_END, t::K_AND])?;
        let mut out: Vec<(Literal<'a>, Literal<'a>)> = Vec::new();
        for (k, v) in entries {
            match (k, v) {
                (Term::Literal(k), Term::Literal(v)) => {
                    if out.iter().any(|(ok, _)| ok.text == k.text) {
                        self.custom_error(format!("Multiple definition for property {}", k.text))?;
                    }
                    out.push((k, v));
                }
                (Term::Literal(k), v) => {
                    let mut msg = format!(
                        "Invalid property value: {} for property: {}",
                        crate::display::cassandra_text(&v),
                        crate::display::cassandra_text(&Term::Literal(k))
                    );
                    if matches!(v, Term::Bind(_)) {
                        msg.push_str(" (bind variables are not supported in DDL queries)");
                    }
                    self.custom_error(msg)?;
                    break;
                }
                (k, _) => {
                    let mut msg = format!("Invalid property name: {}", crate::display::cassandra_text(&k));
                    if matches!(k, Term::Bind(_)) {
                        msg.push_str(" (bind variables are not supported in DDL queries)");
                    }
                    self.custom_error(msg)?;
                    break;
                }
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------------------------
    // Users
    // -----------------------------------------------------------------------------------------

    /// `username: IDENT | STRING_LITERAL | QUOTED_NAME` (quoted names rejected).
    fn username(&mut self) -> PResult<RoleName<'a>> {
        let ty = self.la(1);
        match ty {
            t::IDENT | t::STRING_LITERAL => {}
            t::QUOTED_NAME => {
                let tok = self.consume();
                let msg = match self.d {
                    Dialect::Cassandra => {
                        "Quoted strings are are not supported for user names and USER is deprecated, please use ROLE"
                    }
                    Dialect::Scylla => "Quoted strings are not supported for user names",
                };
                self.custom_error(msg)?;
                return Ok(RoleName { text: self.text(&tok), form: NameForm::Quoted });
            }
            _ if self.scylla() && t::is_unreserved(ty, self.d) => {}
            _ => return self.no_viable(0),
        }
        let tok = self.consume();
        Ok(if ty == t::STRING_LITERAL {
            RoleName { text: self.text(&tok), form: NameForm::String }
        } else {
            RoleName { text: Cow::Borrowed(&self.src[tok.start as usize..tok.end as usize]), form: NameForm::Ident }
        })
    }

    /// `WITH [HASHED] PASSWORD 'p'` (no `=`).
    fn user_password(&mut self) -> PResult<Option<UserPassword<'a>>> {
        if !self.eat(t::K_WITH) {
            return Ok(None);
        }
        if self.scylla() {
            self.expect_f(t::K_PASSWORD, &[t::STRING_LITERAL])?;
            let tok = self.expect_f(t::STRING_LITERAL, &[F_END])?;
            return Ok(Some(UserPassword::Plain(self.text(&tok))));
        }
        match self.la(1) {
            t::K_PASSWORD => {
                self.consume();
                let tok = self.expect_f(t::STRING_LITERAL, &[F_END, t::K_SUPERUSER, t::K_NOSUPERUSER])?;
                Ok(Some(UserPassword::Plain(self.text(&tok))))
            }
            t::K_HASHED if self.cassandra() => {
                self.consume();
                self.expect(t::K_PASSWORD)?;
                let tok = self.expect(t::STRING_LITERAL)?;
                Ok(Some(UserPassword::Hashed(self.text(&tok))))
            }
            _ => self.no_viable(0),
        }
    }

    fn superuser_flag(&mut self) -> Option<bool> {
        match self.la(1) {
            t::K_SUPERUSER => {
                self.consume();
                Some(true)
            }
            t::K_NOSUPERUSER => {
                self.consume();
                Some(false)
            }
            _ => None,
        }
    }

    pub(crate) fn create_user(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.expect(t::K_USER)?;
        let if_exists = self.ddl_if_not_exists()?;
        let name = self.username()?;
        let password = self.user_password()?;
        let superuser = self.superuser_flag();
        Ok(StatementKind::CreateUser(Box::new(UserStatement { if_exists, name, password, superuser })))
    }

    pub(crate) fn alter_user(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.expect(t::K_USER)?;
        let if_exists = if self.cassandra() { self.ddl_if_exists()? } else { false };
        let name = self.username()?;
        let password = self.user_password()?;
        let superuser = self.superuser_flag();
        Ok(StatementKind::AlterUser(Box::new(UserStatement { if_exists, name, password, superuser })))
    }

    pub(crate) fn drop_user(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_USER)?;
        let if_exists = self.ddl_if_exists()?;
        let name = self.username()?;
        Ok(StatementKind::DropUser(Box::new(DropRole { if_exists, name })))
    }

    pub(crate) fn list_users(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_LIST)?;
        self.expect(t::K_USERS)?;
        Ok(StatementKind::ListUsers)
    }

    /// `identity: IDENT | STRING_LITERAL | QUOTED_NAME` (quoted names rejected).
    fn identity(&mut self) -> PResult<RoleName<'a>> {
        match self.la(1) {
            t::IDENT => {
                let tok = self.consume();
                Ok(RoleName { text: Cow::Borrowed(&self.src[tok.start as usize..tok.end as usize]), form: NameForm::Ident })
            }
            t::STRING_LITERAL => {
                let tok = self.consume();
                Ok(RoleName { text: self.text(&tok), form: NameForm::String })
            }
            t::QUOTED_NAME => {
                let tok = self.consume();
                self.custom_error("Quoted strings are are not supported for identity")?;
                Ok(RoleName { text: self.text(&tok), form: NameForm::Quoted })
            }
            _ => self.no_viable(0),
        }
    }

    pub(crate) fn add_identity(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ADD)?;
        self.expect(t::K_IDENTITY)?;
        let if_not_exists = self.ddl_if_not_exists()?;
        let identity = self.identity()?;
        self.expect(t::K_TO)?;
        self.expect(t::K_ROLE)?;
        let role = self.identity()?;
        Ok(StatementKind::AddIdentity(Box::new(AddIdentity { if_not_exists, identity, role })))
    }

    pub(crate) fn drop_identity(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.expect(t::K_IDENTITY)?;
        let if_exists = self.ddl_if_exists()?;
        let identity = self.identity()?;
        Ok(StatementKind::DropIdentity(Box::new(DropIdentity { if_exists, identity })))
    }

    // -----------------------------------------------------------------------------------------
    // ScyllaDB service levels
    // -----------------------------------------------------------------------------------------

    /// `SERVICE LEVEL` or `SERVICE_LEVEL`.
    fn service_level_keyword(&mut self) -> PResult<()> {
        if self.eat(t::K_SERVICE_LEVEL) {
            return Ok(());
        }
        if !self.at(t::K_SERVICE) {
            return self.no_viable(0);
        }
        self.consume();
        self.expect_f(t::K_LEVEL, &[F_END])?;
        Ok(())
    }

    /// `SERVICE LEVELS` or `SERVICE_LEVELS`.
    fn service_levels_keyword(&mut self) -> PResult<()> {
        if self.eat(t::K_SERVICE_LEVELS) {
            return Ok(());
        }
        if !self.at(t::K_SERVICE) {
            return self.no_viable(0);
        }
        self.consume();
        self.expect_f(t::K_LEVELS, &[F_END])?;
        Ok(())
    }

    pub(crate) fn create_service_level(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_CREATE)?;
        self.service_level_keyword()?;
        let if_exists = self.ddl_if_not_exists()?;
        let name = self.role_or_service_level_name(true)?;
        let properties = if self.eat(t::K_WITH) { self.properties()? } else { Vec::new() };
        Ok(StatementKind::CreateServiceLevel(Box::new(ServiceLevelStatement { if_exists, name, properties })))
    }

    pub(crate) fn alter_service_level(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ALTER)?;
        self.service_level_keyword()?;
        let if_exists = self.ddl_if_exists()?;
        let name = self.role_or_service_level_name(true)?;
        self.expect_f(t::K_WITH, &[F_IDENT])?;
        let properties = self.properties()?;
        Ok(StatementKind::AlterServiceLevel(Box::new(ServiceLevelStatement { if_exists, name, properties })))
    }

    pub(crate) fn drop_service_level(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DROP)?;
        self.service_level_keyword()?;
        let if_exists = self.ddl_if_exists()?;
        let name = self.role_or_service_level_name(true)?;
        Ok(StatementKind::DropServiceLevel(Box::new(DropRole { if_exists, name })))
    }

    pub(crate) fn attach_service_level(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_ATTACH)?;
        self.service_level_keyword()?;
        let role = self.role_or_service_level_name(true)?;
        self.expect_f(t::K_TO, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        let grantee = self.role_name()?;
        Ok(StatementKind::AttachServiceLevel(Box::new(RoleGrant { role, grantee })))
    }

    pub(crate) fn detach_service_level(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_DETACH)?;
        self.service_level_keyword()?;
        self.expect_f(t::K_FROM, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
        Ok(StatementKind::DetachServiceLevel(self.role_name()?))
    }

    pub(crate) fn list_service_levels(&mut self) -> PResult<StatementKind<'a>> {
        self.expect(t::K_LIST)?;
        let out = match self.la(1) {
            t::K_ALL => {
                self.consume();
                if self.eat(t::K_ATTACHED) {
                    self.service_levels_keyword()?;
                    ListServiceLevels::AllAttached
                } else {
                    self.service_levels_keyword()?;
                    ListServiceLevels::All
                }
            }
            t::K_ATTACHED => {
                self.consume();
                self.service_level_keyword()?;
                self.expect_f(t::K_OF, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
                ListServiceLevels::AttachedOf(self.role_name()?)
            }
            t::K_EFFECTIVE => {
                self.consume();
                self.service_level_keyword()?;
                self.expect_f(t::K_OF, &[F_IDENT, t::STRING_LITERAL, t::QMARK])?;
                ListServiceLevels::EffectiveOf(self.role_name()?)
            }
            _ => {
                self.service_level_keyword()?;
                ListServiceLevels::One(self.role_or_service_level_name(true)?)
            }
        };
        Ok(StatementKind::ListServiceLevels(Box::new(out)))
    }
}

/// `DCPermissions.Builder` / `CIDRPermissions.Builder` state: `ALL` and a list are exclusive.
#[derive(Default)]
struct BuilderState {
    all: bool,
    some: bool,
}

impl BuilderState {
    fn set_all(&mut self) -> bool {
        if self.some {
            return false;
        }
        self.all = true;
        true
    }

    fn add(&mut self) -> bool {
        if self.all {
            return false;
        }
        self.some = true;
        true
    }
}

/// `IRoleManager.Option` names, for duplicate detection.
fn option_key(o: &RoleOption<'_>) -> Option<&'static str> {
    Some(match o {
        RoleOption::Password(_) => "PASSWORD",
        RoleOption::HashedPassword(_) => "HASHED_PASSWORD",
        RoleOption::Options(_) => "OPTIONS",
        RoleOption::Superuser(_) => "SUPERUSER",
        RoleOption::Login(_) => "LOGIN",
        _ => return None,
    })
}

/// ScyllaDB's resource display in error messages: `<table ks.t>`, `<all roles>`, ...
fn scylla_resource_display(r: &Resource<'_>) -> String {
    match r {
        Resource::AllKeyspaces => "<all keyspaces>".into(),
        Resource::Keyspace(k) => format!("<keyspace {}>", k.name()),
        Resource::Table { name, .. } => match &name.keyspace {
            Some(_) => format!("<table {}>", super::ddl::qualified_display(name)),
            None => format!("<table .{}>", name.name.name()),
        },
        Resource::AllTablesInKeyspace(k) => format!("<all tables in {}>", k.name()),
        Resource::AllRoles => "<all roles>".into(),
        Resource::Role(r) => format!("<role {}>", r.name()),
        Resource::AllFunctions => "<all functions>".into(),
        Resource::AllFunctionsInKeyspace(k) => format!("<all functions in {}>", k.name()),
        Resource::Function { name, arg_types } => {
            let ks = name.keyspace.as_ref().map(|k| format!("{}.", k.name())).unwrap_or_default();
            let mut args = String::new();
            for a in arg_types {
                args.push_str(&crate::display::type_text(a));
                args.push(',');
            }
            format!("<function {ks}{}({args})>", name.resolved())
        }
        Resource::AllMbeans => "<all mbeans>".into(),
        Resource::Mbean { name, .. } => format!("<mbean {name}>"),
    }
}
