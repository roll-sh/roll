//! CREATE / ALTER / DROP EXTENSION for the extensions whose scripts the engine runs
//! (`crate::ext`), as extension.c does: the version to install and the update path to it,
//! required extensions (CASCADE), the scripts run with PostgreSQL's settings, and the objects
//! they create recorded as the extension's members (pg_depend deptype 'e').

use super::Ddl;
use crate::catalog::depend::*;
use crate::catalog::{Catalog, ExtensionInfo, FIRST_USER_OID, PG_CATALOG_OID, quote_ident};
use crate::error::{Error, Frame, Result, code};
use crate::ext::{self, Control};
use crate::sql::binder::ast::{const_str, inner, names};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::collections::BTreeSet;
use std::sync::Arc;

thread_local! {
    /// The extension whose script is running (`CurrentExtensionObject`), 0 when none.
    static CREATING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The extension whose script is running on this thread (`creating_extension`), if any.
pub(crate) fn creating_extension() -> Option<u32> {
    let e = CREATING.get();
    (e != 0).then_some(e)
}

pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    if CREATING.get() != 0 {
        out.push("ddl::CREATING_EXTENSION");
    }
}

/// Every object of the catalog that can be an extension's member, by (class, oid): what
/// `recordDependencyOnCurrentExtension` is called for (not array types, a table's row
/// type, indexes, constraints, triggers, rules or defaults).
pub(crate) fn member_candidates(cat: &Catalog) -> BTreeSet<(u32, u32)> {
    let mut out = BTreeSet::new();
    let user = |o: &u32| *o >= FIRST_USER_OID;
    out.extend(cat.functions.keys().filter(|o| user(o)).map(|o| (PROC, *o)));
    for (o, s) in &cat.schemas {
        if user(o) && !s.name.starts_with("pg_temp") && !s.name.starts_with("pg_toast_temp") {
            out.insert((NAMESPACE, *o));
        }
    }
    for set in [
        cat.base_types.keys().copied().collect::<Vec<_>>(),
        cat.enums.keys().copied().collect(),
        cat.domains.keys().copied().collect(),
        cat.composites.keys().copied().collect(),
        cat.ranges.keys().copied().collect(),
    ] {
        out.extend(set.into_iter().filter(user).map(|o| (TYPE, o)));
    }
    out.extend(cat.tables.keys().filter(|o| user(o)).map(|o| (REL, *o)));
    out.extend(cat.views.keys().filter(|o| user(o)).map(|o| (REL, *o)));
    out.extend(cat.sequences.keys().filter(|o| user(o)).map(|o| (REL, *o)));
    out.extend(cat.operators.keys().filter(|o| user(o)).map(|o| (OPERATOR, *o)));
    out.extend(cat.opclasses.keys().filter(|o| user(o)).map(|o| (OPCLASS, *o)));
    out.extend(cat.opfamilies.keys().filter(|o| user(o)).map(|o| (OPFAMILY, *o)));
    out.extend(cat.casts.keys().filter(|o| user(o)).map(|o| (CAST, *o)));
    out.extend(cat.access_methods.keys().filter(|o| user(o)).map(|o| (AM, *o)));
    out.extend(cat.collations.keys().filter(|o| user(o)).map(|o| (COLLATION, *o)));
    out.extend(cat.conversions.keys().filter(|o| user(o)).map(|o| (CONVERSION, *o)));
    out.extend(cat.languages.keys().filter(|o| user(o)).map(|o| (LANGUAGE, *o)));
    out.extend(cat.transforms.keys().filter(|o| user(o)).map(|o| (TRANSFORM, *o)));
    out.extend(cat.fdws.keys().filter(|o| user(o)).map(|o| (FDW, *o)));
    out.extend(cat.servers.keys().filter(|o| user(o)).map(|o| (SERVER, *o)));
    out.extend(cat.event_triggers.keys().filter(|o| user(o)).map(|o| (EVENT_TRIGGER, *o)));
    out.extend(cat.ts_dicts.keys().filter(|o| user(o)).map(|o| (TSDICT, *o)));
    out.extend(cat.ts_templates.keys().filter(|o| user(o)).map(|o| (TSTEMPLATE, *o)));
    out.extend(cat.ts_parsers.keys().filter(|o| user(o)).map(|o| (TSPARSER, *o)));
    out.extend(cat.ts_configs.keys().filter(|o| user(o)).map(|o| (TSCONFIG, *o)));
    out
}

/// Forget members (and configuration tables) that no longer exist.
pub(crate) fn prune_members(cat: &mut Catalog) {
    if cat.extension_info.is_empty() {
        return;
    }
    let live = member_candidates(cat);
    let exts: BTreeSet<u32> = cat.extensions.iter().map(|e| e.0).collect();
    cat.extension_info.retain(|o, _| exts.contains(o));
    for info in cat.extension_info.values_mut() {
        info.members.retain(|m| live.contains(m));
        info.config.retain(|(t, _)| live.contains(&(REL, *t)));
    }
}

/// The extension `(class, oid)` is a member of.
pub(crate) fn owning_extension(cat: &Catalog, obj: (u32, u32)) -> Option<u32> {
    cat.extension_info
        .iter()
        .find(|(_, i)| i.members.contains(&obj))
        .map(|(o, _)| *o)
}

/// The text of `defname`'s argument (`defGetString`).
fn def_string(d: &pb::DefElem) -> Option<String> {
    let arg = d.arg.as_deref()?;
    match inner(arg) {
        Some(NodeEnum::String(s)) => Some(s.sval.clone()),
        Some(NodeEnum::TypeName(t)) => Some(names(&t.names).join(".")),
        Some(NodeEnum::Integer(i)) => Some(i.ival.to_string()),
        Some(NodeEnum::Boolean(b)) => Some(if b.boolval { "true" } else { "false" }.into()),
        _ => const_str(arg),
    }
}

impl Ddl<'_, '_> {
    /// The oid of installed extension `name`.
    fn extension_oid(&self, name: &str) -> Option<u32> {
        if name == "plpgsql" {
            return Some(PLPGSQL_EXT);
        }
        self.cat.extensions.iter().find(|e| e.1 == name).map(|e| e.0)
    }

    /// CREATE EXTENSION (`CreateExtension`).
    pub(super) fn create_extension_script(&mut self, s: &pb::CreateExtensionStmt) -> Result<String> {
        let name = s.extname.clone();
        ext::check_extension_name(&name)?;
        if self.extension_oid(&name).is_some() {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_OBJECT,
                    format!("extension \"{name}\" already exists, skipping"),
                );
                return Ok("CREATE EXTENSION".into());
            }
            return Err(Error::new(code::DUPLICATE_OBJECT, format!("extension \"{name}\" already exists")));
        }
        if creating_extension().is_some() {
            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "nested CREATE EXTENSION is not supported"));
        }
        let (mut schema, mut version, mut cascade) = (None, None, None);
        for o in &s.options {
            let Some(NodeEnum::DefElem(d)) = inner(o) else {
                continue;
            };
            let slot = match d.defname.as_str() {
                "schema" => &mut schema,
                "new_version" => &mut version,
                "cascade" => &mut cascade,
                other => return Err(crate::error::internal(format!("unrecognized option: {other}"))),
            };
            if slot.is_some() {
                return Err(crate::sql::binder::ast::with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
            *slot = Some(def_string(d).unwrap_or_default());
        }
        let cascade = cascade.is_some_and(|c| c != "false" && c != "0");
        self.create_extension_internal(&name, schema.as_deref(), version, cascade, &[], true)?;
        Ok("CREATE EXTENSION".into())
    }

    /// `CreateExtensionInternal`: install `name` (and, with `cascade`, what it requires).
    fn create_extension_internal(
        &mut self,
        name: &str,
        schema_name: Option<&str>,
        version: Option<String>,
        cascade: bool,
        parents: &[String],
        is_create: bool,
    ) -> Result<u32> {
        let pcontrol = ext::control(name).ok_or_else(|| ext::not_available(name))?;
        let version = match version.or_else(|| pcontrol.default_version.clone()) {
            Some(v) => v,
            None => {
                return Err(Error::new(code::INVALID_PARAMETER_VALUE, "version to install must be specified"));
            }
        };
        ext::check_version_name(&version)?;
        // The script installing that version, or the best one to start from and the updates
        // after it.
        let (version, updates) = if ext::script(name, None, &version).is_some() {
            (version, Vec::new())
        } else {
            let mut g = ext::version_graph(name);
            let target = ext::version_index(&mut g, &version);
            match ext::install_path(&g, target) {
                Some((start, path)) => (g[start].name.clone(), path),
                None => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!(
                            "extension \"{name}\" has no installation script nor update path for version \"{version}\""
                        ),
                    ));
                }
            }
        };
        let control = pcontrol;
        // The schema: as given (it must exist), the control file's (made if missing), or the
        // first of the search path.
        let mut schema_oid = match schema_name {
            Some(s) => Some(self.cat.schema_oid(s)?),
            None => None,
        };
        let schema = if let Some(cs) = &control.schema {
            if schema_name.is_some_and(|s| s != cs) && !cascade {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("extension \"{}\" must be installed in schema \"{cs}\"", control.name),
                ));
            }
            schema_oid = match self.cat.schema_by_name(cs) {
                Some(sc) => Some(sc.oid),
                None => {
                    let stmt = pb::CreateSchemaStmt {
                        schemaname: cs.clone(),
                        ..Default::default()
                    };
                    self.create_schema(&stmt)?;
                    Some(self.cat.schema_oid(cs)?)
                }
            };
            cs.clone()
        } else {
            match schema_oid {
                Some(o) => self.cat.schema_name(o).to_string(),
                None => {
                    let o = self.creation_schema().map_err(|_| {
                        Error::new(code::UNDEFINED_SCHEMA, "no schema has been selected to create in")
                    })?;
                    schema_oid = Some(o);
                    self.cat.schema_name(o).to_string()
                }
            }
        };
        let schema_oid = schema_oid.unwrap_or(PG_CATALOG_OID);
        // Prerequisites, installed first with CASCADE.
        let mut required = Vec::new();
        let mut required_schemas = Vec::new();
        for req in &control.requires {
            let r = self.required_extension(req, name, schema_name, cascade, parents, is_create)?;
            let rs = self.cat.extensions.iter().find(|e| e.0 == r).map_or(PG_CATALOG_OID, |e| e.2);
            required.push(r);
            required_schemas.push((req.clone(), self.cat.schema_name(rs).to_string()));
        }
        // InsertExtensionTuple, and the control file's comment.
        let oid = self.cat.alloc_oid();
        self.cat
            .extensions
            .push((oid, name.to_string(), schema_oid, version.clone()));
        self.cat.extension_info.insert(
            oid,
            ExtensionInfo {
                owner: self.ctx.current_user(),
                relocatable: control.relocatable,
                config: Vec::new(),
                members: Vec::new(),
                requires: required,
            },
        );
        if let Some(c) = &control.comment {
            self.cat.comments.retain(|(o, s, _)| !(*o == oid && *s == 0));
            self.cat.comments.push((oid, 0, c.clone()));
        }
        self.save()?;
        self.execute_extension_script(oid, control, None, &version, &required_schemas, &schema)?;
        self.apply_extension_updates(oid, control, &version, &updates, schema_name, cascade, is_create)?;
        Ok(oid)
    }

    /// `get_required_extension`.
    fn required_extension(
        &mut self,
        req: &str,
        name: &str,
        orig_schema: Option<&str>,
        cascade: bool,
        parents: &[String],
        is_create: bool,
    ) -> Result<u32> {
        if let Some(o) = self.extension_oid(req) {
            return Ok(o);
        }
        if !cascade {
            let mut e = Error::new(code::UNDEFINED_OBJECT, format!("required extension \"{req}\" is not installed"));
            if is_create {
                e = e.hint("Use CREATE EXTENSION ... CASCADE to install required extensions too.");
            }
            return Err(e);
        }
        ext::check_extension_name(req)?;
        if parents.iter().any(|p| p == req) {
            return Err(Error::new(
                code::INVALID_RECURSION,
                format!("cyclic dependency detected between extensions \"{req}\" and \"{name}\""),
            ));
        }
        self.notice(format!("installing required extension \"{req}\""));
        let mut parents = parents.to_vec();
        parents.push(name.to_string());
        self.create_extension_internal(req, orig_schema, None, cascade, &parents, is_create)
    }

    /// `ApplyExtensionUpdates`: run each update script in turn, the pg_extension row moving
    /// to its version first.
    fn apply_extension_updates(
        &mut self,
        oid: u32,
        control: &Control,
        initial: &str,
        updates: &[String],
        orig_schema: Option<&str>,
        cascade: bool,
        is_create: bool,
    ) -> Result<()> {
        let mut old = initial.to_string();
        for v in updates {
            let (schema_oid, name) = {
                let e = self
                    .cat
                    .extensions
                    .iter_mut()
                    .find(|e| e.0 == oid)
                    .ok_or_else(|| crate::error::internal("extension disappeared"))?;
                e.3 = v.clone();
                (e.2, e.1.clone())
            };
            if let Some(info) = self.cat.extension_info.get_mut(&oid) {
                info.relocatable = control.relocatable;
            }
            let mut required = Vec::new();
            let mut required_schemas = Vec::new();
            for req in &control.requires {
                let r = self.required_extension(req, &name, orig_schema, cascade, &[], is_create)?;
                let rs = self.cat.extensions.iter().find(|e| e.0 == r).map_or(PG_CATALOG_OID, |e| e.2);
                required.push(r);
                required_schemas.push((req.clone(), self.cat.schema_name(rs).to_string()));
            }
            if let Some(info) = self.cat.extension_info.get_mut(&oid) {
                info.requires = required;
            }
            self.save()?;
            let schema = self.cat.schema_name(schema_oid).to_string();
            self.execute_extension_script(oid, control, Some(&old), v, &required_schemas, &schema)?;
            old = v.clone();
        }
        Ok(())
    }

    /// `execute_extension_script`: the install script of `version` (or the update from
    /// `from`) with PostgreSQL's substitutions, run statement by statement with
    /// client_min_messages at least WARNING, check_function_bodies off and the search path
    /// set to the extension's schema (and its prerequisites'), the objects it creates
    /// becoming the extension's members.
    fn execute_extension_script(
        &mut self,
        ext_oid: u32,
        control: &Control,
        from: Option<&str>,
        version: &str,
        required_schemas: &[(String, String)],
        schema: &str,
    ) -> Result<()> {
        // A non-superuser runs a trusted extension's script as the bootstrap superuser.
        let mut switch = false;
        if control.superuser && !self.is_superuser() {
            if control.trusted && self.require_database_priv(crate::catalog::acl::CREATE).is_ok() {
                switch = true;
            } else {
                let (what, hint) = if from.is_none() { ("create", "create") } else { ("update", "update") };
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to {what} extension \"{}\"", control.name),
                )
                .hint(if control.trusted {
                    format!("Must have CREATE privilege on current database to {hint} this extension.")
                } else {
                    format!("Must be superuser to {hint} this extension.")
                }));
            }
        }
        let Some((file, text)) = ext::script(&control.name, from, version) else {
            let f = match from {
                Some(f) => format!("{}--{f}--{version}.sql", control.name),
                None => format!("{}--{version}.sql", control.name),
            };
            return Err(Error::new(
                code::UNDEFINED_FILE,
                format!("could not open file \"/usr/local/share/postgresql/extension/{f}\" for reading: No such file or directory"),
            ));
        };
        let sql = self.script_text(text, control, required_schemas, schema);
        // The settings the script runs with, as a function's SET options hold.
        let settings = &self.ctx.sess.settings;
        let level = settings.borrow_mut().push_nest();
        struct Pop<'a>(&'a std::cell::RefCell<crate::session::settings::Settings>, usize);
        impl Drop for Pop<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().pop_nest(self.1);
            }
        }
        let _pop = Pop(settings, level);
        {
            let mut st = settings.borrow_mut();
            let verbose = !matches!(st.get_str("client_min_messages"), Some("warning" | "error"));
            if verbose {
                st.set_saved("client_min_messages", "warning")?;
            }
            if st.get_str("check_function_bodies") != Some("off") {
                st.set_saved("check_function_bodies", "off")?;
            }
            let mut path = quote_ident(schema);
            for (_, rs) in required_schemas {
                if rs != "pg_catalog" {
                    path.push_str(", ");
                    path.push_str(&quote_ident(rs));
                }
            }
            path.push_str(", pg_temp");
            st.set_saved("search_path", &path)?;
        }
        let definer = &self.ctx.sess.definer;
        if switch {
            definer.borrow_mut().push(crate::catalog::BOOTSTRAP_SUPERUSER_OID);
        }
        struct Undefine<'a>(Option<&'a std::cell::RefCell<Vec<u32>>>);
        impl Drop for Undefine<'_> {
            fn drop(&mut self) {
                if let Some(d) = self.0 {
                    d.borrow_mut().pop();
                }
            }
        }
        let _undefine = Undefine(switch.then_some(definer));
        let _creating = crate::scoped::set(&CREATING, ext_oid, "ddl::CREATING_EXTENSION");
        self.run_script(ext_oid, &file, &sql)?;
        self.cat = (*self.ctx.catalog()).clone();
        Ok(())
    }

    /// The script with `\echo` lines emptied and `@extowner@`, `@extschema@` (of a
    /// non-relocatable extension), `@extschema:name@` and `MODULE_PATHNAME` replaced.
    fn script_text(&self, text: &str, control: &Control, required_schemas: &[(String, String)], schema: &str) -> String {
        let mut sql: String = text
            .split_inclusive('\n')
            .map(|l| if l.starts_with("\\echo") { if l.ends_with('\n') { "\n" } else { "" } } else { l })
            .collect();
        if sql.contains("@extowner@") {
            let me = self.ctx.current_user();
            let owner = self.cat.roles.get(&me).map(|r| r.name.clone()).unwrap_or_default();
            sql = sql.replace("@extowner@", &quote_ident(&owner));
        }
        if !control.relocatable {
            sql = sql.replace("@extschema@", &quote_ident(schema));
        }
        for (req, rs) in required_schemas {
            sql = sql.replace(&format!("@extschema:{req}@"), &quote_ident(rs));
        }
        if let Some(m) = &control.module_pathname {
            sql = sql.replace("MODULE_PATHNAME", m);
        }
        sql
    }

    /// `execute_sql_string`: each statement of a script run in turn (queries' rows thrown
    /// away), what it creates joining extension `ext_oid`. Errors and notices name the
    /// statement and the line it is near.
    fn run_script(&mut self, ext_oid: u32, file: &str, sql: &str) -> Result<()> {
        let stmts = crate::sql::parse::parse(sql)?;
        for raw in &stmts {
            let Some(node) = raw.stmt.as_deref().and_then(|n| n.node.as_ref()) else {
                continue;
            };
            let start = raw.stmt_location.max(0) as usize;
            let end = if raw.stmt_len > 0 {
                (start + raw.stmt_len as usize).min(sql.len())
            } else {
                sql.len()
            };
            let lead = sql[start..end].len() - sql[start..end].trim_start().len();
            let text = sql[start..end].trim();
            let line = 1 + sql[..start + lead].matches('\n').count();
            if let NodeEnum::TransactionStmt(_) = node {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "transaction control statements are not allowed within an extension script",
                ));
            }
            let before = member_candidates(&self.ctx.catalog());
            let sess = self.ctx.sess;
            sess.framed(Frame::Text(format!("extension script file \"{file}\", near line {line}")), |_| {
                sess.framed(
                    Frame::Query {
                        kind: "SQL statement",
                        text: text.to_string(),
                        shift: -((start + lead) as isize),
                    },
                    |_| self.run_script_statement(node, sql),
                )
            })?;
            // What the statement created is the extension's.
            let mut cat = (*self.ctx.catalog()).clone();
            let after = member_candidates(&cat);
            let info = cat.extension_info.entry(ext_oid).or_default();
            for m in after.difference(&before) {
                if !info.members.contains(m) {
                    info.members.push(*m);
                }
            }
            info.members.retain(|m| after.contains(m));
            self.ctx.txn.borrow_mut().set_catalog(cat)?;
        }
        Ok(())
    }

    fn run_script_statement(&self, node: &NodeEnum, sql: &str) -> Result<()> {
        let ctx = self.ctx.refreshed();
        let r = match node {
            NodeEnum::SelectStmt(_)
            | NodeEnum::InsertStmt(_)
            | NodeEnum::UpdateStmt(_)
            | NodeEnum::DeleteStmt(_)
            | NodeEnum::MergeStmt(_)
                if !matches!(node, NodeEnum::UpdateStmt(u) if super::catalog_dml::is_pg_class_update(u)) =>
            {
                let mut b = crate::sql::binder::Binder::new(&ctx, Vec::new());
                let bq = b.bind_statement(&pb::Node { node: Some(node.clone()) })?;
                crate::sql::binder::perms::check(&bq.perms, &ctx)?;
                let plan = Arc::new(bq.plan);
                ctx.new_statement(|| crate::exec::collect(&plan, &ctx, None)).map(|_| ())
            }
            n => super::run_nested(|| super::execute(&ctx, n, sql)).map(|_| ()),
        };
        self.ctx
            .notices
            .borrow_mut()
            .extend(ctx.notices.borrow_mut().drain(..));
        r
    }

    /// ALTER EXTENSION ... UPDATE [TO version] (`ExecAlterExtensionStmt`).
    pub(super) fn alter_extension(&mut self, s: &pb::AlterExtensionStmt) -> Result<String> {
        if creating_extension().is_some() {
            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "nested ALTER EXTENSION is not supported"));
        }
        let Some((oid, _, _, old)) = self.cat.extensions.iter().find(|e| e.1 == s.extname).cloned() else {
            return Err(Error::new(code::UNDEFINED_OBJECT, format!("extension \"{}\" does not exist", s.extname)));
        };
        let owner = self.cat.extension_info.get(&oid).map_or(crate::catalog::BOOTSTRAP_SUPERUSER_OID, |i| i.owner);
        self.require_owner(owner, "extension", &s.extname)?;
        let control = ext::control(&s.extname).ok_or_else(|| ext::not_available(&s.extname))?;
        let mut new_version = None;
        for o in &s.options {
            let Some(NodeEnum::DefElem(d)) = inner(o) else {
                continue;
            };
            if d.defname != "new_version" {
                return Err(crate::error::internal(format!("unrecognized option: {}", d.defname)));
            }
            if new_version.is_some() {
                return Err(crate::sql::binder::ast::with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
            new_version = Some(def_string(d));
        }
        let version = match new_version.flatten().or_else(|| control.default_version.clone()) {
            Some(v) => v,
            None => {
                return Err(Error::new(code::INVALID_PARAMETER_VALUE, "version to install must be specified"));
            }
        };
        ext::check_version_name(&version)?;
        if version == old {
            self.notice(format!("version \"{version}\" of extension \"{}\" is already installed", s.extname));
            return Ok("ALTER EXTENSION".into());
        }
        let mut g = ext::version_graph(&s.extname);
        let start = ext::version_index(&mut g, &old);
        let target = ext::version_index(&mut g, &version);
        let Some(path) = ext::update_path(&g, start, target, false) else {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "extension \"{}\" has no update path from version \"{old}\" to version \"{version}\"",
                    s.extname
                ),
            ));
        };
        self.apply_extension_updates(oid, control, &old, &path, None, false, false)?;
        Ok("ALTER EXTENSION".into())
    }

    /// ALTER EXTENSION ... ADD / DROP object (`ExecAlterExtensionContentsStmt`).
    pub(super) fn alter_extension_contents(&mut self, s: &pb::AlterExtensionContentsStmt) -> Result<String> {
        let Some((ext_oid, ext_name, _, _)) = self.cat.extensions.iter().find(|e| e.1 == s.extname).cloned()
        else {
            return Err(Error::new(code::UNDEFINED_OBJECT, format!("extension \"{}\" does not exist", s.extname)));
        };
        let owner = self.cat.extension_info.get(&ext_oid).map_or(crate::catalog::BOOTSTRAP_SUPERUSER_OID, |i| i.owner);
        self.require_owner(owner, "extension", &s.extname)?;
        let kind = pb::ObjectType::try_from(s.objtype).unwrap_or(pb::ObjectType::Undefined);
        let (class, oid) = self.object_address(kind, s.object.as_deref())?;
        if class == EXTENSION {
            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot add an extension as a member of an extension"));
        }
        let cat = Arc::new(self.cat.clone());
        let desc = describe(&cat, self.search_path(), (class, oid, 0)).unwrap_or_default();
        let current = owning_extension(&self.cat, (class, oid));
        if s.action > 0 {
            if let Some(cur) = current {
                let cur_name = self.cat.extensions.iter().find(|e| e.0 == cur).map(|e| e.1.clone()).unwrap_or_default();
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("{desc} is already a member of extension \"{cur_name}\""),
                ));
            }
            if class == NAMESPACE && self.cat.extensions.iter().any(|e| e.0 == ext_oid && e.2 == oid) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("cannot add schema \"{}\" to extension \"{ext_name}\" because the schema contains the extension", self.cat.schema_name(oid)),
                ));
            }
            self.cat.extension_info.entry(ext_oid).or_default().members.push((class, oid));
        } else {
            if current != Some(ext_oid) {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("{desc} is not a member of extension \"{ext_name}\""),
                ));
            }
            if let Some(info) = self.cat.extension_info.get_mut(&ext_oid) {
                info.members.retain(|m| *m != (class, oid));
            }
        }
        self.save()?;
        Ok("ALTER EXTENSION".into())
    }

    /// The object ALTER EXTENSION ADD / DROP names (`get_object_address`): (class, oid).
    fn object_address(&mut self, kind: pb::ObjectType, obj: Option<&pb::Node>) -> Result<(u32, u32)> {
        use pb::ObjectType as O;
        let list = || match obj.and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            Some(NodeEnum::String(s)) => vec![s.sval.clone()],
            _ => Vec::new(),
        };
        let unsupported = || crate::error::unsupported("ALTER EXTENSION ADD / DROP of this kind of object");
        Ok(match kind {
            O::ObjectTable | O::ObjectView | O::ObjectMatview | O::ObjectSequence | O::ObjectForeignTable => {
                let parts = list();
                let name = parts.last().cloned().unwrap_or_default();
                let schema = (parts.len() > 1).then(|| parts[parts.len() - 2].clone());
                let r = self
                    .cat
                    .lookup(schema.as_deref(), &name, self.search_path())?
                    .ok_or_else(|| crate::catalog::no_such_relation(schema.as_deref(), &name))?;
                (REL, r.oid())
            }
            O::ObjectSchema => {
                let n = list().pop().unwrap_or_default();
                (NAMESPACE, self.cat.schema_oid(&n)?)
            }
            O::ObjectType | O::ObjectDomain => {
                let Some(NodeEnum::TypeName(tn)) = obj.and_then(inner) else {
                    return Err(unsupported());
                };
                (TYPE, self.type_oid(&tn.names)?)
            }
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                let Some(NodeEnum::ObjectWithArgs(owa)) = obj.and_then(inner) else {
                    return Err(unsupported());
                };
                match self.find_routine(owa, kind)? {
                    Some(f) => (PROC, f.oid),
                    None => return Err(self.no_routine(owa, kind)),
                }
            }
            O::ObjectOperator => {
                let Some(NodeEnum::ObjectWithArgs(owa)) = obj.and_then(inner) else {
                    return Err(unsupported());
                };
                match self.operator_object(owa, false)? {
                    Some(o) => (OPERATOR, o.oid),
                    None => return Err(unsupported()),
                }
            }
            O::ObjectOpclass | O::ObjectOpfamily => {
                let (am, parts) = self.opc_object(&list())?;
                if kind == O::ObjectOpfamily {
                    (OPFAMILY, self.opfamily_named(&am, &parts)?.oid)
                } else {
                    (OPCLASS, self.opclass_named(&am, &parts)?.oid)
                }
            }
            O::ObjectCast => match obj {
                Some(n) => (CAST, self.cast_to_drop(n, false)?.ok_or_else(unsupported)?),
                None => return Err(unsupported()),
            },
            O::ObjectAccessMethod | O::ObjectLanguage | O::ObjectConversion | O::ObjectTransform => match obj {
                Some(n) => {
                    let (c, o, _) = self.misc_to_drop(kind, n, false)?.ok_or_else(unsupported)?;
                    (c, o)
                }
                None => return Err(unsupported()),
            },
            O::ObjectTsconfiguration | O::ObjectTsdictionary | O::ObjectTsparser | O::ObjectTstemplate => {
                let k = super::tsearch::ts_kind(kind).ok_or_else(unsupported)?;
                let oid = self.ts_object(k, &list(), false)?.ok_or_else(unsupported)?;
                (ts_class(k), oid)
            }
            O::ObjectExtension => {
                let n = list().pop().unwrap_or_default();
                (
                    EXTENSION,
                    self.extension_oid(&n)
                        .ok_or_else(|| Error::new(code::UNDEFINED_OBJECT, format!("extension \"{n}\" does not exist")))?,
                )
            }
            _ => return Err(unsupported()),
        })
    }

    /// DROP EXTENSION's check (`RemoveExtensionById`): not the one whose script is running.
    pub(super) fn check_extension_drop(&self, oid: u32, name: &str) -> Result<()> {
        if creating_extension() == Some(oid) {
            return Err(Error::new(
                code::OBJECT_IN_USE,
                format!("cannot drop extension \"{name}\" because it is being modified"),
            ));
        }
        Ok(())
    }
}
