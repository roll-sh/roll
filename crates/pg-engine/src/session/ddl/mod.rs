//! DDL: schema changes run inside the transaction. Each statement edits a copy of the catalog
//! and stores it in the transaction, so DDL commits or rolls back with everything else.

mod composite;
mod inherit;
mod objects;
mod partition;
mod privs;
pub(crate) use objects::EXTENSIONS;
mod aggregate;
mod catalog_dml;
pub(crate) use catalog_dml::{
    apply_depend_update, depend_update_query, is_pg_class_update, is_pg_depend_delete,
    is_pg_depend_insert, is_pg_depend_update, merge_pg_class,
    might_merge_pg_class, pg_depend_dml,
};
mod collation;
mod analyze;
mod domain;
mod basetype;
mod opclass;
mod operator;
mod policy;
mod rule;
pub(crate) mod event_trigger;
pub(crate) mod extension;
mod stats;
pub(crate) mod stats_import;
mod table;
mod trigger;
mod userobj;
mod fdw;
mod foreign;
mod publication;
mod subscription;
mod tblspc;
mod stored;
mod tsearch;
mod vacuum;

pub(crate) use stored::mentions;
pub(crate) use table::rowid_sequence;

use crate::catalog::{Catalog, Relation, Table};
use crate::error::{Error, Result, code};
use crate::exec::scan::decode_table_row;
use crate::exec::{ExecCtx, Row};
use crate::sql::binder::Binder;
use crate::sql::binder::ast::inner;
use crate::storage::keys::{index_prefix, prefix_end};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const NAMESPACE_CLASS: u32 = 2615;
const TYPE_CLASS: u32 = 1247;
const PROC_CLASS: u32 = 1255;
const FDW_CLASS: u32 = 2328;
const ROLE_CLASS: u32 = 1260;

/// Whether `oid` is a user type of `c`: an enum, composite, range, multirange or domain.
fn before_has_type(c: &Catalog, oid: u32) -> bool {
    c.enums.contains_key(&oid)
        || c.composites.contains_key(&oid)
        || c.ranges.contains_key(&oid)
        || c.domains.contains_key(&oid)
        || c.ranges.values().any(|r| r.multi_oid == oid)
}

thread_local! {
    /// While a client's utility statement runs on this thread: whether it is inside a
    /// transaction block. None: not at top level (inside a function).
    static TOP_LEVEL: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Run `f` as a client's top-level utility statement, `in_block` or not.
pub(crate) fn run_top_level<T>(in_block: bool, f: impl FnOnce() -> T) -> T {
    let _g = crate::scoped::set(&TOP_LEVEL, Some(in_block), "ddl::TOP_LEVEL");
    f()
}

/// Run `f` as a utility statement inside a function (not at top level).
pub(crate) fn run_nested<T>(f: impl FnOnce() -> T) -> T {
    let _g = crate::scoped::set(&TOP_LEVEL, None, "ddl::TOP_LEVEL.nested");
    f()
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    extension::unsettled(out);
    if TOP_LEVEL.get().is_some() {
        out.push("ddl::TOP_LEVEL");
    }
}

/// PreventInTransactionBlock: `what` runs only at top level, outside a transaction block.
pub(crate) fn prevent_in_block(what: &str) -> Result<()> {
    match TOP_LEVEL.with(|t| t.get()) {
        Some(false) => Ok(()),
        Some(true) => Err(Error::new(
            code::ACTIVE_SQL_TRANSACTION,
            format!("{what} cannot run inside a transaction block"),
        )),
        None => Err(Error::new(
            code::ACTIVE_SQL_TRANSACTION,
            format!("{what} cannot be executed from a function"),
        )),
    }
}

/// Run one DDL (or other utility) statement; returns the command tag.
/// `sql` is the statement's text, for positions inside function bodies ("" when unknown).
pub fn execute(ctx: &ExecCtx, node: &NodeEnum, sql: &str) -> Result<String> {
    match node {
        NodeEnum::NotifyStmt(s) => {
            crate::notify::check(&s.conditionname, &s.payload)?;
            ctx.txn.borrow_mut().notify(&s.conditionname, &s.payload);
            return Ok("NOTIFY".into());
        }
        NodeEnum::ListenStmt(s) => {
            ctx.sess
                .listen_actions
                .borrow_mut()
                .push((true, Some(s.conditionname.clone())));
            return Ok("LISTEN".into());
        }
        NodeEnum::UnlistenStmt(s) => {
            ctx.sess.listen_actions.borrow_mut().push((
                false,
                (!s.conditionname.is_empty()).then(|| s.conditionname.clone()),
            ));
            return Ok("UNLISTEN".into());
        }
        // SET in a routine (EXECUTE, a SQL function's body, an extension script): the
        // statements after it see the value.
        NodeEnum::VariableSetStmt(s) => {
            crate::session::routine_set(ctx, s)?;
            return Ok("SET".into());
        }
        _ => {}
    }
    if let Some(r) = crate::session::cursor::spi_cursor_stmt(ctx, node, sql, &[]) {
        return r;
    }
    crate::sql::binder::note_catalog(&ctx.catalog());
    // Event triggers fire around the command (ddl_command_start, sql_drop, ddl_command_end).
    let evt = event_trigger::begin(ctx, node)?;
    let mut d = Ddl::new(ctx, sql);
    let tag = match d.run(node) {
        Ok(t) => t,
        Err(e) => {
            if let Some(b) = evt {
                event_trigger::abort(b);
            }
            return Err(e);
        }
    };
    if let Some(b) = evt {
        event_trigger::finish(ctx, b, node)?;
    }
    Ok(tag)
}

/// `execute`, for a statement whose `$n` (with these types) are the context's parameters.
pub fn execute_with_params(
    ctx: &ExecCtx,
    node: &NodeEnum,
    params: &[crate::types::DataType],
) -> Result<String> {
    let mut d = Ddl::new(ctx, "");
    d.params = params.to_vec();
    d.run(node)
}

/// CreateTableAsRelExists: EXPLAIN of CREATE TABLE AS checks its target before planning;
/// true (after a notice) when IF NOT EXISTS finds it taken.
pub fn ctas_rel_exists(ctx: &ExecCtx, s: &pb::CreateTableAsStmt) -> Result<bool> {
    let Some(rv) = s.into.as_ref().and_then(|i| i.rel.as_ref()) else {
        return Ok(false);
    };
    let mut d = Ddl::new(ctx, "");
    let (schema, name) = d.create_target(rv)?;
    if !d.cat.name_taken(schema, &name) {
        return Ok(false);
    }
    if !s.if_not_exists {
        return Err(Error::new(
            code::DUPLICATE_TABLE,
            format!("relation \"{name}\" already exists"),
        ));
    }
    d.notice_code(
        code::DUPLICATE_TABLE,
        format!("relation \"{name}\" already exists, skipping"),
    );
    Ok(true)
}

pub(crate) struct Ddl<'c, 'a> {
    pub ctx: &'c ExecCtx<'a>,
    /// LOCK TABLE ... NOWAIT: the relation locks it takes don't wait.
    pub lock_nowait: bool,
    pub cat: Catalog,
    /// The catalog `cat` was copied from (the base of the transaction's catalog change).
    base: Arc<Catalog>,
    pub sql: &'c str,
    /// The types of `$n` in the statement (CREATE TABLE AS EXECUTE: the prepared one's).
    pub params: Vec<crate::types::DataType>,
    /// Changing a typed table as part of ALTER TYPE ... CASCADE (its own column changes are
    /// refused otherwise).
    pub typed_cascade: bool,
    /// OIDs taken ahead for the sequences of the table being created (Postgres creates them
    /// before the table).
    pub seq_oids: std::collections::VecDeque<u32>,
    /// ALTER TABLE ONLY: subcommands don't recurse to inheriting children.
    pub only: bool,
    /// Tables the ALTER TABLE rewrites into a new heap, which happens at its end
    /// (ATRewriteTables).
    pub rewrites: Vec<u32>,
    /// Why each table in `rewrites` is rewritten (AT_REWRITE_* bits, for table_rewrite
    /// event triggers).
    pub rewrite_why: Vec<(u32, i32)>,
    /// What ALTER COLUMN TYPE re-creates (ATPostAlterTypeCleanup), per altered table in
    /// work-queue order: the constraints (owner table, oid) in changedConstraintOids order,
    /// and the plain indexes.
    pub type_rebuilds: Vec<table::TypeRebuild>,
    /// Extended statistics ALTER COLUMN TYPE re-creates (in the last pass).
    pub stat_rebuilds: Vec<u32>,
    /// CREATE FOREIGN TABLE: what the table `create_table` makes is (taken by it).
    pub creating_foreign: Option<crate::catalog::ForeignTableDef>,
    /// CREATE TABLE: generated columns whose expressions are checked once every column
    /// exists (column name, expression as written).
    pub defer_generated: Option<Vec<(String, Option<pb::Node>)>>,
    /// CREATE TABLE: column defaults, cooked once the relation exists (cookDefault in
    /// AddRelationNewConstraints): (column, expression as written, its type).
    pub defer_defaults: Option<Vec<(String, pb::Node, crate::types::DataType)>>,
    /// ALTER COLUMN TYPE: NOTICEs of constraints made anew merging with a child's, sent
    /// once the statement's constraints are all back (ATPostAlterTypeCleanup).
    pub merge_notices: Vec<String>,
    /// An ALTER TABLE of several ALTER COLUMN TYPEs (and DROP COLUMNs): the table as the
    /// statement found it, which every USING reads, and the conversions (column, over that
    /// row) made in its one rewrite at the end (ATRewriteTable).
    pub combined_types: Option<(Arc<crate::catalog::Table>, Vec<(usize, crate::sql::expr::Expr)>)>,
    /// ATTACH PARTITION matching the partition's indexes to its parent's
    /// (AttachPartitionEnsureIndexes): an invalid one isn't taken. (CREATE INDEX on the
    /// parent takes it, and is invalid itself then.)
    pub attaching: bool,
}

impl<'c, 'a> Ddl<'c, 'a> {
    fn new(ctx: &'c ExecCtx<'a>, sql: &'c str) -> Self {
        let base = ctx.catalog();
        Ddl {
            ctx,
            cat: (*base).clone(),
            base,
            sql,
            typed_cascade: false,
            seq_oids: Default::default(),
            only: false,
            rewrites: Vec::new(),
            rewrite_why: Vec::new(),
            type_rebuilds: Vec::new(),
            stat_rebuilds: Vec::new(),
            params: Vec::new(),
            creating_foreign: None,
            defer_generated: None,
            defer_defaults: None,
            merge_notices: Vec::new(),
            combined_types: None,
            attaching: false,
            lock_nowait: false,
        }
    }

    fn run(&mut self, node: &NodeEnum) -> Result<String> {
        use NodeEnum as N;
        // An object put in `pg_temp` creates the session's temporary schema first.
        let d = format!("{node:?}");
        let names_temp =
            d.contains("sval: \"pg_temp\"") || d.contains("schemaname: \"pg_temp\"");
        // (A statement naming one of the session's temporary tables uses it too.)
        let temp_rel = || {
            let Some(t) = self.cat.schema_by_name("pg_temp").map(|s| s.oid) else {
                return false;
            };
            d.split("relname: \"")
                .skip(1)
                .chain(d.split("sval: \"").skip(1))
                .any(|rest| {
                    let name = rest.split('"').next().unwrap_or("");
                    self.cat.relation_in(t, name).is_some()
                })
        };
        if names_temp || d.contains("relpersistence: \"t\"") || temp_rel() {
            self.ctx.sess.temp_used.set(true);
            self.ctx.sess.temp_ever.set(true);
        }
        if !matches!(node, N::CreateStmt(_) | N::ViewStmt(_) | N::CreateSeqStmt(_))
            && names_temp
            && self.cat.schema_by_name("pg_temp").is_none()
        {
            self.temp_schema()?;
        }
        Ok(match node {
            N::CreateStmt(s) => {
                let rv = s.relation.as_ref().unwrap();
                let parent = if s.partbound.is_some() {
                    s.inh_relations.first().and_then(|n| match inner(n) {
                        Some(NodeEnum::RangeVar(p)) => self.lookup(p).ok().flatten().map(|r| r.oid()),
                        _ => None,
                    })
                } else {
                    None
                };
                let am = self.new_rel_am(&s.access_method, s.partspec.is_some(), parent)?;
                let tag = self.create_table(s)?;
                if am.is_some()
                    && let Some(r) = self.lookup(rv)?
                {
                    self.set_rel_am(r.oid(), am)?;
                }
                tag
            }
            N::CreateTableAsStmt(s) => {
                let explicit = s.into.as_ref().map(|i| i.access_method.clone()).unwrap_or_default();
                let am = self.new_rel_am(&explicit, false, None)?;
                let tag = self.create_table_as(s)?;
                if am.is_some()
                    && let Some(rv) = s.into.as_ref().and_then(|i| i.rel.as_ref())
                    && let Some(r) = self.lookup(rv)?
                {
                    self.set_rel_am(r.oid(), am)?;
                }
                tag
            }
            N::AlterTableStmt(s) => {
                let n = s
                    .cmds
                    .iter()
                    .filter(|c| {
                        matches!(inner(c), Some(NodeEnum::AlterTableCmd(c))
                            if c.subtype == pb::AlterTableType::AtSetAccessMethod as i32)
                    })
                    .count();
                if n > 1 {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "cannot have multiple SET ACCESS METHOD subcommands",
                    ));
                }
                // ALTER MATERIALIZED VIEW ... SET ACCESS METHOD.
                if n == 1
                    && s.cmds.len() == 1
                    && s.objtype == pb::ObjectType::ObjectMatview as i32
                    && let Some(rv) = s.relation.as_ref()
                    && let Some(Relation::View(v)) = self.lookup(rv)?
                    && v.materialized
                {
                    self.require_owner(v.owner, "materialized view", &v.name)?;
                    let name = match inner(&s.cmds[0]) {
                        Some(NodeEnum::AlterTableCmd(c)) => c.name.clone(),
                        _ => String::new(),
                    };
                    let am = if name.is_empty() {
                        self.default_table_am()?
                    } else {
                        self.table_am(&name)?
                    };
                    // A new access method rewrites it (AT_REWRITE_ACCESS_METHOD).
                    if self.cat.rel_am.get(&v.oid).copied().unwrap_or(2) != am {
                        self.rewrite_why.push((v.oid, 8));
                        self.fire_table_rewrite(v.oid)?;
                    }
                    self.set_rel_am(v.oid, (am != 2).then_some(am))?;
                    return Ok("ALTER MATERIALIZED VIEW".into());
                }
                self.alter_table(s)?
            }
            N::RenameStmt(s) => self.rename(s)?,
            N::AlterObjectSchemaStmt(s) => self.set_schema(s)?,
            N::IndexStmt(s) => self.create_index(s)?,
            N::ViewStmt(s) => self.create_view(s)?,
            N::RefreshMatViewStmt(s) => self.refresh_matview(s)?,
            N::CreateSeqStmt(s) => self.create_sequence(s)?,
            N::AlterSeqStmt(s) => self.alter_sequence(s)?,
            N::CreateSchemaStmt(s) => self.create_schema(s)?,
            N::CreateEnumStmt(s) => self.create_enum(s)?,
            N::AlterEnumStmt(s) => self.alter_enum(s)?,
            N::CreateDomainStmt(s) => self.create_domain(s)?,
            N::CompositeTypeStmt(s) => self.create_composite(s)?,
            N::CreateRangeStmt(s) => self.create_range(s)?,
            N::CreateFunctionStmt(s) => self.create_function(s)?,
            N::CreateExtensionStmt(s) if crate::ext::control(&s.extname).is_some() => {
                self.create_extension_script(s)?
            }
            N::CreateExtensionStmt(s) => self.create_extension(s)?,
            N::AlterExtensionStmt(s) => self.alter_extension(s)?,
            N::AlterExtensionContentsStmt(s) => self.alter_extension_contents(s)?,
            N::CommentStmt(s) => self.comment(s)?,
            N::DropStmt(s) => self.drop(s)?,
            N::TruncateStmt(s) => self.truncate(s)?,
            N::CreateRoleStmt(s) => self.create_role(s)?,
            N::AlterRoleStmt(s) => self.alter_role(s)?,
            N::DropRoleStmt(s) => self.drop_role(s)?,
            N::LockStmt(s) => self.lock(s)?,
            N::GrantStmt(g) => self.grant(g)?,
            N::GrantRoleStmt(g) => self.grant_role(g)?,
            N::AlterDefaultPrivilegesStmt(s) => self.alter_default_privileges(s)?,
            N::AlterOwnerStmt(s) => self.alter_owner(s)?,
            N::ReassignOwnedStmt(s) => self.reassign_owned(s)?,
            N::DropOwnedStmt(s) => self.drop_owned(s)?,
            N::AlterDatabaseSetStmt(s) => self.alter_database_set(s)?,
            N::AlterDatabaseStmt(_) => "ALTER DATABASE".into(),
            N::AlterRoleSetStmt(s) => self.alter_role_set(s)?,
            N::VacuumStmt(s) => self.vacuum(s)?,
            N::ClusterStmt(s) => self.cluster(s)?,
            N::ReindexStmt(s) => self.reindex(s)?,
            N::CreateStatsStmt(s) => self.create_statistics(s)?,
            N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectOperator as i32 => {
                self.create_operator(s)?
            }
            N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectAggregate as i32 => {
                self.create_aggregate(s)?
            }
            N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectCollation as i32 => {
                self.create_collation(s)?
            }
            N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectType as i32 => {
                self.define_base_type(s)?
            }
            N::AlterTypeStmt(s) => self.alter_base_type(s)?,
            N::AlterCollationStmt(s) => self.alter_collation(s)?,
            N::AlterDatabaseRefreshCollStmt(s) => self.refresh_database_collation(&s.dbname)?,
            N::DefineStmt(s)
                if tsearch::ts_kind(pb::ObjectType::try_from(s.kind).unwrap_or_default())
                    .is_some() =>
            {
                self.create_ts(
                    tsearch::ts_kind(pb::ObjectType::try_from(s.kind).unwrap_or_default()).unwrap(),
                    s,
                )?
            }
            N::AlterTsdictionaryStmt(s) => self.alter_ts_dictionary(s)?,
            N::AlterTsconfigurationStmt(s) => self.alter_ts_configuration(s)?,
            N::AlterOperatorStmt(s) => self.alter_operator(s)?,
            N::CreateOpFamilyStmt(s) => self.create_opfamily(s)?,
            N::CreateCastStmt(s) => self.create_cast(s)?,
            N::CreateConversionStmt(s) => self.create_conversion(s)?,
            N::CreateAmStmt(s) => self.create_am(s)?,
            N::CreatePlangStmt(s) => self.create_language(s)?,
            N::CreateTransformStmt(s) => self.create_transform(s)?,
            N::CreateOpClassStmt(s) => self.create_opclass(s)?,
            N::AlterOpFamilyStmt(s) => self.alter_opfamily(s)?,
            N::AlterStatsStmt(s) => self.alter_statistics(s)?,
            N::AlterFunctionStmt(s) => self.alter_function(s)?,
            N::AlterDomainStmt(s) => self.alter_domain(s)?,
            N::CreateTrigStmt(s) => self.create_trigger(s)?,
            N::CreatePolicyStmt(s) => self.create_policy(s)?,
            N::AlterPolicyStmt(s) => self.alter_policy(s)?,
            N::UpdateStmt(s) if catalog_dml::is_pg_class_update(s) => self.update_pg_class(s)?,
            N::RuleStmt(s) => self.create_rule(s)?,
            N::CreateEventTrigStmt(s) => self.create_event_trigger(s)?,
            N::AlterEventTrigStmt(s) => self.alter_event_trigger(s)?,
            N::CreatePublicationStmt(s) => self.create_publication(s)?,
            N::AlterPublicationStmt(s) => self.alter_publication(s)?,
            N::CreateSubscriptionStmt(s) => self.create_subscription(s)?,
            N::AlterSubscriptionStmt(s) => self.alter_subscription(s)?,
            N::DropSubscriptionStmt(s) => self.drop_subscription(s)?,
            N::AlterSystemStmt(_) => {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "permission denied to set parameter via ALTER SYSTEM",
                ));
            }
            N::CreateTableSpaceStmt(s) => self.create_tablespace(s)?,
            N::DropTableSpaceStmt(s) => self.drop_tablespace(s)?,
            N::AlterTableSpaceOptionsStmt(s) => self.alter_tablespace_options(s)?,
            N::AlterTableMoveAllStmt(s) => self.move_all(s)?,
            N::CreateFdwStmt(s) => self.create_fdw(s)?,
            N::AlterFdwStmt(s) => self.alter_fdw(s)?,
            N::CreateForeignServerStmt(s) => self.create_server(s)?,
            N::AlterForeignServerStmt(s) => self.alter_server(s)?,
            N::CreateUserMappingStmt(s) => self.create_user_mapping(s)?,
            N::AlterUserMappingStmt(s) => self.alter_user_mapping(s)?,
            N::DropUserMappingStmt(s) => self.drop_user_mapping(s)?,
            N::CreateForeignTableStmt(s) => self.create_foreign_table(s)?,
            N::ImportForeignSchemaStmt(s) => self.import_foreign_schema(s)?,
            N::SecLabelStmt(s) => {
                // No label provider is ever loaded.
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    if s.provider.is_empty() {
                        "no security label providers have been loaded".to_string()
                    } else {
                        format!("security label provider \"{}\" is not loaded", s.provider)
                    },
                ));
            }
            _ => return Err(crate::error::unsupported("this statement")),
        })
    }

    /// Start over from the transaction's catalog as it is now.
    pub(super) fn reload_catalog(&mut self) {
        self.base = self.ctx.catalog();
        self.cat = (*self.base).clone();
    }

    /// After running user code that may itself run DDL (a function a partition bound
    /// calls): when the transaction's catalog is no longer `before`, carry this command's
    /// edits so far onto it, so neither its changes nor the nested commands' are lost.
    pub(super) fn absorb_nested(&mut self, before: &Arc<Catalog>) -> Result<()> {
        let cur = self.ctx.catalog();
        if cur.version == before.version {
            return Ok(());
        }
        self.cat = crate::txn::catmerge::merge(&self.base, &self.cat, &cur)?;
        self.base = cur;
        Ok(())
    }

    /// Store the edited catalog in the transaction.
    fn save(&mut self) -> Result<()> {
        self.lock_references()?;
        self.prepare_save();
        let before = self.ctx.catalog();
        self.cat.dodge_array_type_names_since(&before);
        self.check_new_type_usage()?;
        self.ctx.txn.borrow_mut().set_catalog_from(self.cat.clone(), &self.base)
    }

    /// Lock database object (`class`, `oid`) in `mode` (LockDatabaseObject; a role's
    /// is a shared object's, LockSharedObject) until the transaction ends.
    pub(super) fn lock_object(&self, class: u32, oid: u32, mode: crate::txn::locks::LockMode) -> Result<()> {
        let db = if class == ROLE_CLASS { 0 } else { self.ctx.sess.db.oid };
        let key = crate::txn::locks::object_key(db, class, oid, 0);
        self.ctx
            .txn
            .borrow_mut()
            .lock(&key, mode, crate::txn::locks::WaitPolicy::Block, &|| format!("object {oid}"))?;
        Ok(())
    }

    /// recordDependencyOn's LockNotPinnedObject (and shdepend's for roles): what the
    /// objects this command created or changed refer to is locked (AccessShareLock), so a
    /// concurrent DROP of it waits, and checked to be there still once that lock is held.
    fn lock_references(&mut self) -> Result<()> {
        let before = self.ctx.txn.borrow().catalog();
        // (What other transactions had committed before this waits.)
        let committed = self.ctx.sess.db.catalog();
        let mut refs: Vec<(u32, u32)> = Vec::new();
        let mut add = |class: u32, oid: u32| {
            if oid >= crate::catalog::FIRST_USER_OID && !refs.contains(&(class, oid)) {
                refs.push((class, oid));
            }
        };
        let ty = |t: &crate::types::DataType| crate::catalog::system::type_oid(&self.cat, t);
        // (Only what this command changed: the maps' differences, not every object.)
        for (_, _, f) in before.functions.changes(&self.cat.functions) {
            let Some(f) = f else { continue };
            add(NAMESPACE_CLASS, f.schema);
            for (i, (_, t)) in f.args.iter().enumerate() {
                add(TYPE_CLASS, f.arg_domains.get(i).copied().flatten().unwrap_or_else(|| ty(t)));
            }
            add(TYPE_CLASS, f.ret_domain.unwrap_or_else(|| ty(&f.ret)));
            add(ROLE_CLASS, f.owner);
            if f.sql_body.is_some() {
                for (class, o, _) in crate::catalog::depend::sql_body_refs(self.ctx, &before, f) {
                    if class == PROC_CLASS {
                        add(PROC_CLASS, o);
                    }
                }
            }
        }
        for (oid, d) in self.cat.domains.iter() {
            if before.domains.get(oid).is_some_and(|b| Arc::ptr_eq(b, d)) {
                continue;
            }
            add(NAMESPACE_CLASS, d.schema);
            add(TYPE_CLASS, d.parent.or(d.base_domain).unwrap_or_else(|| ty(&d.base)));
        }
        for (oid, _, t) in before.tables.changes(&self.cat.tables) {
            let Some(t) = t.filter(|t| !t.temporary) else { continue };
            // (A table's columns: only one this command made.)
            if before.tables.contains_key(&oid) {
                continue;
            }
            add(NAMESPACE_CLASS, t.schema);
            for c in t.columns.iter().filter(|c| !c.dropped) {
                add(TYPE_CLASS, c.domain.unwrap_or_else(|| ty(&c.ty)));
            }
        }
        for (oid, sv) in self.cat.servers.iter() {
            if before.servers.get(oid).is_some_and(|b| Arc::ptr_eq(b, sv)) {
                continue;
            }
            add(FDW_CLASS, sv.fdw);
        }
        // (Types made with this command's objects, a table's row type, aren't others'.)
        refs.retain(|(class, o)| *class != TYPE_CLASS || before_has_type(&before, *o));
        for (class, oid) in refs {
            self.lock_object(class, oid, crate::txn::locks::LockMode::AccessShare)?;
            let now = self.ctx.sess.db.catalog();
            let exists = |c: &Catalog| match class {
                NAMESPACE_CLASS => c.schemas.contains_key(&oid),
                TYPE_CLASS => before_has_type(c, oid),
                PROC_CLASS => c.functions.contains_key(&oid),
                FDW_CLASS => c.fdws.contains_key(&oid),
                ROLE_CLASS => c.roles.contains_key(&oid),
                _ => true,
            };
            // (Dropped by another transaction meanwhile: committed before the wait, gone
            // after it. One this transaction made is in neither.)
            if !exists(&now) && exists(&committed) {
                return Err(match class {
                    ROLE_CLASS => Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("role {oid} was concurrently dropped"),
                    ),
                    _ => Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "referenced {} was concurrently dropped",
                            match class {
                                NAMESPACE_CLASS => "schema",
                                TYPE_CLASS => "type",
                                PROC_CLASS => "function",
                                _ => "foreign-data wrapper",
                            }
                        ),
                    ),
                });
            }
        }
        Ok(())
    }

    /// USAGE on a type a command stores (aclcheck_error_type): `ty`, or domain `domain`.
    pub(super) fn require_type_usage(
        &self,
        ty: &crate::types::DataType,
        domain: Option<u32>,
    ) -> Result<()> {
        if self.is_superuser() {
            return Ok(());
        }
        let elem_ty = match ty {
            crate::types::DataType::Array(e) if domain.is_none() => e.as_ref(),
            t => t,
        };
        let oid = domain.unwrap_or_else(|| crate::catalog::system::type_oid(&self.cat, elem_ty));
        if oid < crate::catalog::FIRST_USER_OID {
            return Ok(());
        }
        // A multirange has its range type's privileges.
        let acl_oid = self
            .cat
            .ranges
            .values()
            .find(|r| r.multi_oid == oid)
            .map_or(oid, |r| r.oid);
        let (p, _) = self.cat.obj_mask(
            crate::catalog::acl::ObjKind::Type,
            acl_oid,
            self.me(),
            self.db_owner(),
        );
        if p & crate::catalog::acl::USAGE == 0 {
            let name = self.cat.format_type(
                &crate::types::DataType::Unknown,
                Some(oid),
                self.search_path(),
            );
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for type {name}"),
            ));
        }
        Ok(())
    }

    /// check_usage_on_types (and the commands' own checks of the types they store): each
    /// dependency the statement adds on a user type needs USAGE on it (an array's: on its
    /// element type).
    fn check_new_type_usage(&self) -> Result<()> {
        use crate::catalog::depend::{REL, TYPE, depends};
        if self.is_superuser() {
            return Ok(());
        }
        let before = self.ctx.catalog();
        let after = Arc::new(self.cat.clone());
        let on_type = |d: &crate::catalog::depend::Depend| {
            d.refobj.0 == TYPE && d.kind == 'n' && d.refobj.1 >= crate::catalog::FIRST_USER_OID
        };
        // A constraint is known by its table and name: one made anew from its definition
        // (ALTER COLUMN TYPE's rebuild) isn't a new use of a type.
        let ident = |cat: &crate::catalog::Catalog, obj: crate::catalog::depend::Addr| {
            if obj.0 == crate::catalog::depend::CONSTRAINT
                && let Some((t, c)) = cat.tables.values().find_map(|t| {
                    t.constraints.iter().find(|c| c.oid == obj.1).map(|c| (t.oid, c.name.clone()))
                })
            {
                return (obj.0, t, c);
            }
            (obj.0, obj.1, obj.2.to_string())
        };
        let fresh: Vec<crate::catalog::depend::Depend> = {
            let new: Vec<_> = depends(self.ctx, &after).into_iter().filter(on_type).collect();
            if new.is_empty() {
                return Ok(());
            }
            let old: std::collections::HashSet<_> = depends(self.ctx, &before)
                .into_iter()
                .filter(on_type)
                .map(|d| (ident(&before, d.obj), d.refobj))
                .collect();
            new.into_iter()
                .filter(|d| !old.contains(&(ident(&after, d.obj), d.refobj)))
                .collect()
        };
        let me = self.me();
        let db_owner = self.db_owner();
        for d in fresh {
            let oid = d.refobj.1;
            let c = &self.cat;
            let elem = c
                .enums
                .values()
                .map(|e| (e.oid, e.array_oid))
                .chain(c.composites.values().map(|x| (x.oid, x.array_oid)))
                .chain(c.domains.values().map(|x| (x.oid, x.array_oid)))
                .chain(c.base_types.values().map(|b| (b.ty.oid, b.ty.array_oid)))
                .chain(c.ranges.values().map(|r| (r.oid, r.array_oid)))
                .chain(c.ranges.values().map(|r| (r.multi_oid, r.multi_array_oid)))
                .find(|(_, a)| *a == oid && *a != 0)
                .map_or(oid, |(e, _)| e);
            // A multirange has its range type's privileges.
            let acl_oid = c
                .ranges
                .values()
                .find(|r| r.multi_oid == elem)
                .map_or(elem, |r| r.oid);
            let (p, _) = c.obj_mask(crate::catalog::acl::ObjKind::Type, acl_oid, me, db_owner);
            if p & crate::catalog::acl::USAGE == 0 {
                // (An index's expressions are recorded under a restricted search_path.)
                let index = d.obj.0 == REL && c.indexes.contains_key(&d.obj.1);
                let path: &[String] = if index { &[] } else { self.search_path() };
                let name = c.format_type(&crate::types::DataType::Unknown, Some(elem), path);
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for type {name}"),
                ));
            }
        }
        Ok(())
    }

    /// Save with in-place catalog updates only (plain VACUUM's), which take no transaction
    /// id.
    fn save_inplace(&mut self) -> Result<()> {
        self.prepare_save();
        self.ctx.txn.borrow_mut().set_catalog_inplace(self.cat.clone(), &self.base)
    }

    fn prepare_save(&mut self) {
        extension::prune_members(&mut self.cat);
        let before = self.ctx.catalog();
        self.qualify_new_exprs(&before);
        self.add_toast_tables(&before);
        self.apply_default_privileges(&before);
        self.prune_acls(&before);
        self.prune_colstats();
        self.keep_had_triggers(&before);
        self.stamp_frozen_xids(&before);
        // A changed foreign-data wrapper or server row goes last in its catalog's heap.
        {
            let next = self
                .cat
                .fdws
                .values()
                .map(|f| f.heap_order)
                .chain(self.cat.servers.values().map(|x| x.heap_order))
                .max()
                .unwrap_or(0)
                + 1;
            let mut n = next;
            for (oid, f) in self.cat.fdws.clone() {
                if before.fdws.get(&oid).is_none_or(|b| !Arc::ptr_eq(b, &f)) {
                    let mut x = (*f).clone();
                    x.heap_order = n;
                    n += 1;
                    self.cat.fdws.insert(oid, Arc::new(x));
                }
            }
            for (oid, f) in self.cat.servers.clone() {
                if before.servers.get(&oid).is_none_or(|b| !Arc::ptr_eq(b, &f)) {
                    let mut x = (*f).clone();
                    x.heap_order = n;
                    n += 1;
                    self.cat.servers.insert(oid, Arc::new(x));
                }
            }
        }
        // Access methods of relations that are gone.
        if !self.cat.rel_am.is_empty() {
            let cat = &self.cat;
            let live: std::collections::HashSet<u32> = cat
                .rel_am
                .keys()
                .copied()
                .filter(|o| {
                    cat.tables.contains_key(o) || cat.views.contains_key(o) || cat.indexes.contains_key(o)
                })
                .collect();
            self.cat.rel_am.retain(|o, _| live.contains(o));
        }
        // Dropped relations' file numbers go.
        if !self.cat.filenodes.is_empty() {
            let live: std::collections::HashSet<u32> =
                self.cat.storage_oids().into_iter().collect();
            self.cat
                .filenodes
                .retain(|o, _| *o < crate::catalog::FIRST_USER_OID || live.contains(o));
        }
    }

    /// RecentXmin: the oldest transaction still running (ours included), or the next one
    /// to start. New storage gets it as its relfrozenxid; VACUUM's OldestXmin too.
    pub(super) fn recent_xmin(&self) -> u64 {
        let txn = self.ctx.txn.borrow();
        txn.db.xids().snapshot(txn.xid_if_assigned()).xmin
    }

    /// Relations (and TOAST tables) the statement made get RecentXmin as relfrozenxid; the
    /// dropped ones' go.
    ///
    /// Only the relations the statement changed are visited (and the entries it set): the
    /// others are in both catalogs, with storage and entries as before.
    fn stamp_frozen_xids(&mut self, before: &Catalog) {
        // A relation with storage of its own (a table or materialized view): its TOAST table.
        fn rel(c: &Catalog, oid: u32) -> Option<u32> {
            match c.tables.get(&oid) {
                Some(t) => Some(t.toast),
                None => c.views.get(&oid)?.storage.as_ref().map(|t| t.toast),
            }
        }
        let touched: std::collections::BTreeSet<u32> = (before.tables.changes(&self.cat.tables))
            .into_iter()
            .map(|(o, _, _)| o)
            .chain(before.views.changes(&self.cat.views).into_iter().map(|(o, _, _)| o))
            .collect();
        let mut xmin = None;
        for &o in &touched {
            let Some(toast) = rel(&self.cat, o) else { continue };
            let had = rel(before, o);
            for x in [o, toast] {
                // (In the old catalog: as a relation, or as its TOAST table; oids aren't reused.)
                let old = if x == o { had.is_some() } else { had == Some(x) };
                if x != 0 && !old && !self.cat.frozen_xids.contains_key(&x) {
                    let v = *xmin.get_or_insert_with(|| self.recent_xmin());
                    self.cat.frozen_xids.insert(x, v);
                }
            }
        }
        // Entries that may have lost their relation: the changed relations' (and TOAST
        // tables', which only their own relation has), and the others this statement set.
        let mut maybe: Vec<(u32, Option<u32>)> = Vec::new();
        for &o in &touched {
            maybe.push((o, Some(o)));
            if let Some(t) = rel(before, o).filter(|t| *t != 0) {
                maybe.push((t, Some(o)));
            }
        }
        before
            .frozen_xids
            .diff(&self.cat.frozen_xids, |a, b| a == b, |k, _, now| {
                if now.is_some() {
                    maybe.push((k, None))
                }
            });
        let cat = &self.cat;
        let live = |x: u32, owner: Option<u32>| match owner {
            Some(o) if o == x => rel(cat, o).is_some(),
            Some(o) => rel(cat, o) == Some(x),
            None => {
                rel(cat, x).is_some()
                    || touched.iter().any(|o| rel(cat, *o) == Some(x))
                    || cat.toasted().any(|t| t.toast == x)
            }
        };
        let gone: Vec<u32> = maybe
            .into_iter()
            .filter(|(x, o)| cat.frozen_xids.contains_key(x) && !live(*x, *o))
            .map(|(x, _)| x)
            .collect();
        for x in gone {
            self.cat.frozen_xids.remove(&x);
        }
        #[cfg(debug_assertions)]
        {
            let live: std::collections::HashSet<u32> = (self.cat.tables.values())
                .map(|t| (t.oid, t.toast))
                .chain(
                    (self.cat.views.values())
                        .filter_map(|v| v.storage.as_ref().map(|t| (v.oid, t.toast))),
                )
                .flat_map(|(o, t)| [o, t])
                .collect();
            let stale: Vec<u32> =
                self.cat.frozen_xids.keys().copied().filter(|o| !live.contains(o)).collect();
            let old_live: std::collections::HashSet<u32> = (before.tables.values())
                .map(|t| (t.oid, t.toast))
                .chain(
                    (before.views.values())
                        .filter_map(|v| v.storage.as_ref().map(|t| (v.oid, t.toast))),
                )
                .flat_map(|(o, t)| [o, t])
                .collect();
            let old_stale = before.frozen_xids.keys().any(|o| !old_live.contains(o));
            // (A catalog stored by an older build may hold stale entries: they go once
            // their relation is touched again.)
            debug_assert!(old_stale || stale.is_empty(), "stale frozen xids {stale:?}");
            let missing: Vec<u32> = live
                .iter()
                .copied()
                .filter(|o| *o != 0 && !old_live.contains(o) && !self.cat.frozen_xids.contains_key(o))
                .collect();
            debug_assert!(missing.is_empty(), "frozen xids missing for {missing:?}");
        }
    }

    /// A relation's new storage (TRUNCATE, a rewrite) is frozen up to `xid`; a new TOAST
    /// table goes with it.
    pub(super) fn set_frozen_xid(&mut self, t: &Table, xid: u64) {
        self.cat.frozen_xids.insert(t.oid, xid);
        if t.toast != 0 {
            self.cat.frozen_xids.insert(t.toast, xid);
        }
    }

    /// Tables that had triggers before the statement keep relhastriggers (until VACUUM).
    fn keep_had_triggers(&mut self, before: &Catalog) {
        // (Those that still have triggers show relhastriggers anyway: only the tables that
        // lose their last trigger in this statement need remembering, and only the tables
        // it changed, and the ones their foreign keys reference, can.)
        let changed: std::collections::BTreeSet<u32> = (before.tables.changes(&self.cat.tables))
            .into_iter()
            .map(|(o, _, _)| o)
            .collect();
        #[cfg(debug_assertions)]
        let had_in = self.cat.had_triggers.clone();
        let lost = crate::exec::trigger::lost_triggers(before, &self.cat, &changed);
        let cat = &mut self.cat;
        cat.had_triggers.extend(lost);
        for o in &changed {
            if !cat.tables.contains_key(o) {
                cat.had_triggers.remove(o);
            }
        }
        #[cfg(debug_assertions)]
        {
            // As keeping every table that had triggers before the statement would show them.
            let now = crate::exec::trigger::triggered_tables(cat, false);
            let all: Vec<u32> = (had_in.iter().copied())
                .chain(crate::exec::trigger::triggered_tables(before, false))
                .filter(|t| cat.tables.contains_key(t) && !now.contains(t))
                .filter(|t| !cat.had_triggers.contains(t))
                .collect();
            debug_assert!(all.is_empty(), "relhastriggers lost for {all:?}");
        }
    }

    /// Tables the statement made or changed that came to need a TOAST table get one at its
    /// end, in oid order (ALTER TABLE's AlterTableCreateToastTable, CREATE TABLE AS).
    fn add_toast_tables(&mut self, before: &Catalog) {
        let need: Vec<u32> = before
            .tables
            .changes(&self.cat.tables)
            .into_iter()
            .filter_map(|(_, _, t)| t)
            .filter(|t| t.toast == 0 && t.needs_toast())
            .map(|t| t.oid)
            .collect();
        for oid in need {
            let mut t = (*self.cat.tables[&oid]).clone();
            self.cat.add_toast(&mut t);
            self.cat.put_table(t);
        }
    }

    /// pg_statistic rows go with their relation and column (RemoveStatistics).
    fn prune_colstats(&mut self) {
        let cat = &mut self.cat;
        let live = |rel: &u32, attnum: i16| -> bool {
            if let Some(t) = cat
                .tables
                .get(rel)
                .cloned()
                .or_else(|| cat.views.get(rel).and_then(|v| v.storage.clone()))
            {
                return t
                    .columns
                    .get(attnum as usize - 1)
                    .is_some_and(|c| !c.dropped);
            }
            cat.indexes.contains_key(rel)
        };
        let gone: Vec<(u32, i16)> = cat
            .colstats
            .iter()
            .flat_map(|(r, rows)| rows.iter().map(move |s| (*r, s.attnum)))
            .filter(|(r, a)| !live(r, *a))
            .collect();
        for (r, a) in gone {
            if let Some(rows) = cat.colstats.get_mut(&r) {
                rows.retain(|s| s.attnum != a);
                if rows.is_empty() {
                    cat.colstats.remove(&r);
                }
            }
        }
    }

    fn notice(&self, msg: impl Into<String>) {
        self.ctx.notice(super::notice(msg));
    }

    /// A notice with a SQLSTATE ("already exists, skipping" carries the duplicate-object code).
    fn notice_code(&self, c: &'static str, msg: impl Into<String>) {
        let mut e = super::notice(msg);
        e.code = c;
        self.ctx.notice(e);
    }

    /// A binder over the catalog being edited.
    fn binder(&self) -> Binder<'c, 'a> {
        let mut b = Binder::new(self.ctx, self.params.clone());
        b.cat = Arc::new(self.cat.clone());
        b
    }

    fn search_path(&self) -> &[String] {
        &self.ctx.search_path
    }

    /// does_not_exist_skipping for an object on a relation (DROP TRIGGER / POLICY / RULE IF
    /// EXISTS): true, after the NOTICE, when the relation's schema or the relation is missing.
    fn skip_missing_relation(&self, rel: &[String]) -> bool {
        let (schema, table) = match rel {
            [s, t] => (Some(s.as_str()), t.as_str()),
            [t] => (None, t.as_str()),
            _ => return false,
        };
        if let Some(s) = schema
            && self.cat.schema_by_name(s).is_none()
        {
            self.notice(format!("schema \"{s}\" does not exist, skipping"));
            return true;
        }
        if matches!(self.cat.lookup(schema, table, self.search_path()), Ok(None)) {
            self.notice(format!(
                "relation \"{}\" does not exist, skipping",
                rel.join(".")
            ));
            return true;
        }
        false
    }

    /// Where an unqualified non-relation CREATE goes: the first schema of the path, which is
    /// the session's temporary schema when the path names `pg_temp` first.
    fn creation_schema(&self) -> Result<u32> {
        let raw = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("search_path")
            .unwrap_or_default();
        let first = crate::session::settings::parse_search_path(&raw, "");
        if first.first().is_some_and(|s| s == "pg_temp")
            && let Some(t) = self.cat.schema_by_name("pg_temp")
        {
            return Ok(t.oid);
        }
        self.cat.creation_schema(self.search_path())
    }

    /// Schema and name for a new relation. Temporary relations go to the session's schema.
    fn create_target(&mut self, rv: &pb::RangeVar) -> Result<(u32, String)> {
        let temp = rv.relpersistence == "t";
        let name = crate::catalog::truncate_ident(&rv.relname);
        if temp {
            if !rv.schemaname.is_empty() && !rv.schemaname.starts_with("pg_temp") {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    "cannot create temporary relation in non-temporary schema",
                ));
            }
            self.require_temp_create()?;
            return Ok((self.temp_schema()?, name));
        }
        let schema = if rv.schemaname.is_empty() {
            self.cat.creation_schema(self.search_path())?
        } else if rv.schemaname == "pg_temp" {
            self.temp_schema()?
        } else {
            self.cat.schema_oid(&rv.schemaname)?
        };
        if rv.relpersistence == "u"
            && self
                .cat
                .schemas
                .get(&schema)
                .is_some_and(|s| s.name.starts_with("pg_temp") || s.name.starts_with("pg_toast_temp"))
        {
            return Err(crate::sql::binder::ast::with_pos(
                Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    "only temporary relations may be created in temporary schemas",
                ),
                rv.location,
            ));
        }
        if schema == crate::catalog::PG_CATALOG_OID
            || schema == crate::catalog::INFORMATION_SCHEMA_OID
        {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "permission denied to create \"{}.{}\"",
                    self.cat.schema_name(schema),
                    name
                ),
            )
            .detail("System catalog modifications are currently disallowed."));
        }
        if self
            .cat
            .schemas
            .get(&schema)
            .is_some_and(|s| s.name.starts_with("pg_temp"))
        {
            self.require_temp_create()?;
        }
        self.require_schema_create(schema)?;
        Ok((schema, name))
    }

    /// This session's temporary schema, created on first use together with the schema of
    /// its TOAST tables (InitTempTableNamespace).
    fn temp_schema(&mut self) -> Result<u32> {
        let name = self.ctx.sess.temp_schema();
        let toast = name.replacen("pg_temp_", "pg_toast_temp_", 1);
        let mut found = None;
        for n in [name, toast] {
            if let Some(s) = self.cat.schema_by_name(&n) {
                found = found.or(Some(s.oid));
                continue;
            }
            let oid = self.cat.alloc_oid();
            self.cat.schemas.insert(
                oid,
                crate::catalog::Schema {
                    oid,
                    name: n,
                    owner: crate::catalog::BOOTSTRAP_SUPERUSER_OID,
                },
            );
            found = found.or(Some(oid));
        }
        Ok(found.unwrap())
    }

    fn rv_schema(rv: &pb::RangeVar) -> Option<&str> {
        if rv.schemaname.is_empty() {
            None
        } else {
            Some(rv.schemaname.as_str())
        }
    }

    fn lookup(&self, rv: &pb::RangeVar) -> Result<Option<Relation>> {
        if let Some(sc) = Self::rv_schema(rv) {
            self.require_schema_usage(sc)?;
        }
        self.cat
            .lookup(Self::rv_schema(rv), &rv.relname, self.search_path())
    }

    /// The table named by `rv`, or an error naming what it is instead.
    fn table(&self, rv: &pb::RangeVar) -> Result<Arc<Table>> {
        match self.lookup(rv)? {
            Some(Relation::Table(t)) => Ok(t),
            Some(r) => {
                let e = Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table", rv.relname),
                );
                Err(match r {
                    Relation::View(v) if v.materialized => {
                        e.hint("Use ALTER MATERIALIZED VIEW instead.")
                    }
                    Relation::View(_) => e.hint("Use ALTER VIEW instead."),
                    Relation::Sequence(_) => e.hint("Use ALTER SEQUENCE instead."),
                    _ => e,
                })
            }
            None => Err(crate::catalog::no_such_relation(
                Self::rv_schema(rv),
                &rv.relname,
            )),
        }
    }

    /// Every stored row of a table (as of this transaction), with its storage key.
    fn table_rows(&self, t: &Table) -> Result<Vec<(Vec<u8>, Row)>> {
        let start = index_prefix(t.oid, 0);
        let end = prefix_end(&start);
        let entries: Vec<(Vec<u8>, bytes::Bytes)> = self
            .ctx
            .txn
            .borrow()
            .scan(&start, &end)
            .collect::<Result<_>>()?;
        let mut out = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            self.ctx.check()?;
            out.push((k, decode_table_row(t, &v)?));
        }
        Ok(out)
    }

    /// A table's rows with its partitions' (over its columns).
    fn tree_rows(&self, t: &Table) -> Result<Vec<Row>> {
        if t.partkey.is_none() {
            return Ok(self.table_rows(t)?.into_iter().map(|(_, r)| r).collect());
        }
        let mut out = Vec::new();
        for leaf in self.cat.leaves(t.oid) {
            let map: Vec<Option<usize>> = t
                .columns
                .iter()
                .map(|c| {
                    if c.dropped {
                        None
                    } else {
                        leaf.column_index(&c.name)
                    }
                })
                .collect();
            for (_, r) in self.table_rows(&leaf)? {
                out.push(
                    map.iter()
                        .map(|m| m.map_or(crate::types::Value::Null, |j| r[j].clone()))
                        .collect(),
                );
            }
        }
        Ok(out)
    }

    /// Whether a table has any rows.
    fn has_rows(&self, t: &Table) -> Result<bool> {
        let start = index_prefix(t.oid, 0);
        let end = prefix_end(&start);
        Ok(self
            .ctx
            .txn
            .borrow()
            .scan(&start, &end)
            .next()
            .transpose()?
            .is_some())
    }

    fn qualified(&self, schema: u32, name: &str) -> String {
        self.cat.qualified_name(schema, name, self.search_path())
    }
}

/// The command name of a statement that writes (for "cannot execute X in a read-only
/// transaction"); None for statements a read-only transaction may run.
pub(crate) fn write_command(node: &NodeEnum) -> Option<String> {
    use NodeEnum as N;
    let ot = |t: i32| {
        object_type_word(pb::ObjectType::try_from(t).unwrap_or(pb::ObjectType::ObjectTable))
    };
    Some(match node {
        N::InsertStmt(_) => "INSERT".into(),
        N::UpdateStmt(_) => "UPDATE".into(),
        N::DeleteStmt(_) => "DELETE".into(),
        // Only DO NOTHING actions: nothing is written.
        N::MergeStmt(m)
            if m.merge_when_clauses.iter().all(|w| matches!(w.node.as_ref(), Some(N::MergeWhenClause(w)) if w.command_type == pb::CmdType::CmdNothing as i32)) =>
        {
            return None;
        }
        N::MergeStmt(_) => "MERGE".into(),
        N::SelectStmt(s) if crate::sql::binder::ast::select_into(s).is_some() => "SELECT INTO".into(),
        N::CreateTableAsStmt(s) if s.is_select_into => "SELECT INTO".into(),
        N::CreateStmt(_) => "CREATE TABLE".into(),
        N::CreateTableAsStmt(s) if s.objtype == pb::ObjectType::ObjectMatview as i32 => "CREATE MATERIALIZED VIEW".into(),
        N::CreateTableAsStmt(_) => "CREATE TABLE AS".into(),
        N::AlterTableStmt(s) => format!("ALTER {}", ot(s.objtype)),
        N::RenameStmt(s) => format!("ALTER {}", ot(s.rename_type)),
        N::AlterObjectSchemaStmt(s) => format!("ALTER {}", ot(s.object_type)),
        N::IndexStmt(_) => "CREATE INDEX".into(),
        N::ViewStmt(_) => "CREATE VIEW".into(),
        N::RefreshMatViewStmt(_) => "REFRESH MATERIALIZED VIEW".into(),
        N::CreateSeqStmt(_) => "CREATE SEQUENCE".into(),
        N::AlterSeqStmt(_) => "ALTER SEQUENCE".into(),
        N::CreateSchemaStmt(_) => "CREATE SCHEMA".into(),
        N::CreateEnumStmt(_) | N::CompositeTypeStmt(_) | N::CreateRangeStmt(_) => "CREATE TYPE".into(),
        N::AlterEnumStmt(_) => "ALTER TYPE".into(),
        N::CreateDomainStmt(_) => "CREATE DOMAIN".into(),
        N::CreateFunctionStmt(s) => (if s.is_procedure { "CREATE PROCEDURE" } else { "CREATE FUNCTION" }).into(),
        N::CreateExtensionStmt(_) => "CREATE EXTENSION".into(),
        N::CommentStmt(_) => "COMMENT".into(),
        N::DropStmt(s) => format!("DROP {}", ot(s.remove_type)),
        N::TruncateStmt(_) => "TRUNCATE TABLE".into(),
        N::CreateRoleStmt(_) => "CREATE ROLE".into(),
        N::AlterRoleStmt(_) => "ALTER ROLE".into(),
        N::DropRoleStmt(_) => "DROP ROLE".into(),
        N::GrantStmt(g) => (if g.is_grant { "GRANT" } else { "REVOKE" }).into(),
        N::CreatePolicyStmt(_) => "CREATE POLICY".into(),
        N::AlterPolicyStmt(_) => "ALTER POLICY".into(),
        N::CreateTrigStmt(_) => "CREATE TRIGGER".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectOperator as i32 => "CREATE OPERATOR".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectAggregate as i32 => "CREATE AGGREGATE".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectCollation as i32 => "CREATE COLLATION".into(),
        N::AlterCollationStmt(_) => "ALTER COLLATION".into(),
        N::AlterDatabaseRefreshCollStmt(_) => "ALTER DATABASE".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectTsdictionary as i32 => "CREATE TEXT SEARCH DICTIONARY".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectTsconfiguration as i32 => "CREATE TEXT SEARCH CONFIGURATION".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectTsparser as i32 => "CREATE TEXT SEARCH PARSER".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectTstemplate as i32 => "CREATE TEXT SEARCH TEMPLATE".into(),
        N::AlterTsdictionaryStmt(_) => "ALTER TEXT SEARCH DICTIONARY".into(),
        N::AlterTsconfigurationStmt(_) => "ALTER TEXT SEARCH CONFIGURATION".into(),
        N::AlterOperatorStmt(_) => "ALTER OPERATOR".into(),
        N::CreateOpFamilyStmt(_) => "CREATE OPERATOR FAMILY".into(),
        N::CreateOpClassStmt(_) => "CREATE OPERATOR CLASS".into(),
        N::AlterOpFamilyStmt(_) => "ALTER OPERATOR FAMILY".into(),
        N::CreateCastStmt(_) => "CREATE CAST".into(),
        N::CreateFdwStmt(_) => "CREATE FOREIGN DATA WRAPPER".into(),
        N::AlterFdwStmt(_) => "ALTER FOREIGN DATA WRAPPER".into(),
        N::CreateForeignServerStmt(_) => "CREATE SERVER".into(),
        N::AlterForeignServerStmt(_) => "ALTER SERVER".into(),
        N::CreateUserMappingStmt(_) => "CREATE USER MAPPING".into(),
        N::AlterUserMappingStmt(_) => "ALTER USER MAPPING".into(),
        N::DropUserMappingStmt(_) => "DROP USER MAPPING".into(),
        N::CreateConversionStmt(_) => "CREATE CONVERSION".into(),
        N::CreateAmStmt(_) => "CREATE ACCESS METHOD".into(),
        N::CreatePlangStmt(_) => "CREATE LANGUAGE".into(),
        N::CreateTransformStmt(_) => "CREATE TRANSFORM".into(),
        N::DefineStmt(s) if s.kind == pb::ObjectType::ObjectType as i32 => "CREATE TYPE".into(),
        N::AlterTypeStmt(_) => "ALTER TYPE".into(),
        _ => return None,
    })
}

/// `ALTER <word>` / `DROP <word>` for an object type.
pub(crate) fn object_type_word(t: pb::ObjectType) -> &'static str {
    use pb::ObjectType as O;
    match t {
        O::ObjectTable => "TABLE",
        O::ObjectView => "VIEW",
        O::ObjectMatview => "MATERIALIZED VIEW",
        O::ObjectIndex => "INDEX",
        O::ObjectSequence => "SEQUENCE",
        O::ObjectSchema => "SCHEMA",
        O::ObjectType => "TYPE",
        O::ObjectDomain => "DOMAIN",
        O::ObjectFunction => "FUNCTION",
        O::ObjectProcedure => "PROCEDURE",
        O::ObjectRoutine => "ROUTINE",
        O::ObjectAggregate => "AGGREGATE",
        O::ObjectExtension => "EXTENSION",
        O::ObjectDatabase => "DATABASE",
        O::ObjectRole => "ROLE",
        O::ObjectTrigger => "TRIGGER",
        O::ObjectPolicy => "POLICY",
        O::ObjectRule => "RULE",
        O::ObjectCollation => "COLLATION",
        O::ObjectForeignTable => "FOREIGN TABLE",
        O::ObjectTabconstraint => "TABLE",
        O::ObjectColumn => "TABLE",
        O::ObjectPublication => "PUBLICATION",
        O::ObjectSubscription => "SUBSCRIPTION",
        O::ObjectTablespace => "TABLESPACE",
        O::ObjectStatisticExt => "STATISTICS",
        O::ObjectOperator => "OPERATOR",
        O::ObjectTsdictionary => "TEXT SEARCH DICTIONARY",
        O::ObjectLargeobject => "LARGE OBJECT",
        O::ObjectTsconfiguration => "TEXT SEARCH CONFIGURATION",
        O::ObjectTsparser => "TEXT SEARCH PARSER",
        O::ObjectTstemplate => "TEXT SEARCH TEMPLATE",
        O::ObjectOpclass => "OPERATOR CLASS",
        O::ObjectOpfamily => "OPERATOR FAMILY",
        O::ObjectCast => "CAST",
        O::ObjectConversion => "CONVERSION",
        O::ObjectAccessMethod => "ACCESS METHOD",
        O::ObjectLanguage => "LANGUAGE",
        O::ObjectTransform => "TRANSFORM",
        O::ObjectFdw => "FOREIGN DATA WRAPPER",
        O::ObjectForeignServer => "SERVER",
        O::ObjectEventTrigger => "EVENT TRIGGER",
        _ => "OBJECT",
    }
}

/// Dotted name from a list of String nodes, for messages.
pub(crate) fn dotted(names: &[pb::Node]) -> String {
    crate::sql::binder::ast::names(names).join(".")
}

/// The DefElem options of a statement.
pub(crate) fn def_elems(list: &[pb::Node]) -> impl Iterator<Item = &pb::DefElem> {
    list.iter().filter_map(|n| match inner(n) {
        Some(NodeEnum::DefElem(d)) => Some(d.as_ref()),
        _ => None,
    })
}

/// A transaction statement's command name.
pub(crate) fn transaction_tag(t: &pb::TransactionStmt) -> &'static str {
    use pb::TransactionStmtKind as K;
    match K::try_from(t.kind).unwrap_or(K::TransStmtBegin) {
        K::TransStmtStart => "START TRANSACTION",
        K::TransStmtCommit => "COMMIT",
        K::TransStmtRollback | K::TransStmtRollbackTo => "ROLLBACK",
        K::TransStmtSavepoint => "SAVEPOINT",
        K::TransStmtRelease => "RELEASE",
        K::TransStmtPrepare => "PREPARE TRANSACTION",
        K::TransStmtCommitPrepared => "COMMIT PREPARED",
        K::TransStmtRollbackPrepared => "ROLLBACK PREPARED",
        _ => "BEGIN",
    }
}
