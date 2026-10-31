//! Foreign tables (foreigncmds.c, the foreign-table paths of tablecmds.c and
//! parse_utilcmd.c): CREATE FOREIGN TABLE, ALTER FOREIGN TABLE ... OPTIONS, IMPORT FOREIGN
//! SCHEMA, and which ALTER TABLE subcommands a foreign table takes. No FDW handler can run
//! here, so reading or writing a foreign table fails as it does when its wrapper has none.

use super::Ddl;
use crate::catalog::{ForeignTableDef, Table};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{inner, with_pos};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// pg_foreign_table and pg_attribute: the catalogs FDW validators get options for.
const FOREIGN_TABLE_CATALOG: u32 = 3118;
const ATTRIBUTE_CATALOG: u32 = 1249;
/// ERRCODE_FDW_NO_SCHEMAS.
const FDW_NO_SCHEMAS: &str = "HV00P";

/// The error transformCreateStmt / transformAlterTableStmt raise for a constraint a foreign
/// table can't have.
pub(super) fn foreign_constraint_check(c: &pb::Constraint) -> Result<()> {
    use pb::ConstrType as C;
    let what = match C::try_from(c.contype).unwrap_or(C::Undefined) {
        C::ConstrPrimary => "primary key",
        C::ConstrUnique => "unique",
        C::ConstrExclusion => "exclusion",
        C::ConstrForeign => "foreign key",
        _ => return Ok(()),
    };
    Err(with_pos(
        Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("{what} constraints are not supported on foreign tables"),
        ),
        c.location,
    ))
}

/// The constraints of a column definition or table element a foreign table can't have.
pub(super) fn foreign_elements_check(elts: &[pb::Node]) -> Result<()> {
    for e in elts {
        match inner(e) {
            Some(NodeEnum::ColumnDef(cd)) => {
                for c in &cd.constraints {
                    if let Some(NodeEnum::Constraint(c)) = inner(c) {
                        foreign_constraint_check(c)?;
                    }
                }
            }
            Some(NodeEnum::Constraint(c)) => foreign_constraint_check(c)?,
            _ => {}
        }
    }
    Ok(())
}

/// alter_table_type_to_string.
pub(super) fn alter_action(t: pb::AlterTableType) -> Option<&'static str> {
    use pb::AlterTableType as A;
    Some(match t {
        A::AtAddColumn | A::AtAddColumnToView => "ADD COLUMN",
        A::AtColumnDefault | A::AtCookedColumnDefault => "ALTER COLUMN ... SET DEFAULT",
        A::AtDropNotNull => "ALTER COLUMN ... DROP NOT NULL",
        A::AtSetNotNull => "ALTER COLUMN ... SET NOT NULL",
        A::AtSetExpression => "ALTER COLUMN ... SET EXPRESSION",
        A::AtDropExpression => "ALTER COLUMN ... DROP EXPRESSION",
        A::AtSetStatistics => "ALTER COLUMN ... SET STATISTICS",
        A::AtSetOptions => "ALTER COLUMN ... SET",
        A::AtResetOptions => "ALTER COLUMN ... RESET",
        A::AtSetStorage => "ALTER COLUMN ... SET STORAGE",
        A::AtSetCompression => "ALTER COLUMN ... SET COMPRESSION",
        A::AtDropColumn => "DROP COLUMN",
        A::AtAddConstraint | A::AtReAddConstraint | A::AtAddIndexConstraint => "ADD CONSTRAINT",
        A::AtAlterConstraint => "ALTER CONSTRAINT",
        A::AtValidateConstraint => "VALIDATE CONSTRAINT",
        A::AtDropConstraint => "DROP CONSTRAINT",
        A::AtAlterColumnType => "ALTER COLUMN ... SET DATA TYPE",
        A::AtAlterColumnGenericOptions => "ALTER COLUMN ... OPTIONS",
        A::AtChangeOwner => "OWNER TO",
        A::AtClusterOn => "CLUSTER ON",
        A::AtDropCluster => "SET WITHOUT CLUSTER",
        A::AtSetAccessMethod => "SET ACCESS METHOD",
        A::AtSetLogged => "SET LOGGED",
        A::AtSetUnLogged => "SET UNLOGGED",
        A::AtDropOids => "SET WITHOUT OIDS",
        A::AtSetTableSpace => "SET TABLESPACE",
        A::AtSetRelOptions => "SET",
        A::AtResetRelOptions => "RESET",
        A::AtEnableTrig => "ENABLE TRIGGER",
        A::AtEnableAlwaysTrig => "ENABLE ALWAYS TRIGGER",
        A::AtEnableReplicaTrig => "ENABLE REPLICA TRIGGER",
        A::AtDisableTrig => "DISABLE TRIGGER",
        A::AtEnableTrigAll => "ENABLE TRIGGER ALL",
        A::AtDisableTrigAll => "DISABLE TRIGGER ALL",
        A::AtEnableTrigUser => "ENABLE TRIGGER USER",
        A::AtDisableTrigUser => "DISABLE TRIGGER USER",
        A::AtEnableRule => "ENABLE RULE",
        A::AtEnableAlwaysRule => "ENABLE ALWAYS RULE",
        A::AtEnableReplicaRule => "ENABLE REPLICA RULE",
        A::AtDisableRule => "DISABLE RULE",
        A::AtAddInherit => "INHERIT",
        A::AtDropInherit => "NO INHERIT",
        A::AtAddOf => "OF",
        A::AtDropOf => "NOT OF",
        A::AtReplicaIdentity => "REPLICA IDENTITY",
        A::AtEnableRowSecurity => "ENABLE ROW SECURITY",
        A::AtDisableRowSecurity => "DISABLE ROW SECURITY",
        A::AtForceRowSecurity => "FORCE ROW SECURITY",
        A::AtNoForceRowSecurity => "NO FORCE ROW SECURITY",
        A::AtGenericOptions => "OPTIONS",
        A::AtAttachPartition => "ATTACH PARTITION",
        A::AtDetachPartition => "DETACH PARTITION",
        A::AtDetachPartitionFinalize => "DETACH PARTITION ... FINALIZE",
        A::AtAddIdentity => "ALTER COLUMN ... ADD IDENTITY",
        A::AtSetIdentity => "ALTER COLUMN ... SET",
        A::AtDropIdentity => "ALTER COLUMN ... DROP IDENTITY",
        _ => return None,
    })
}

/// errdetail_relkind_not_supported's wording of a relation kind.
pub(super) fn relkind_plural(kind: char) -> &'static str {
    match kind {
        'r' => "tables",
        'i' => "indexes",
        'S' => "sequences",
        't' => "TOAST tables",
        'v' => "views",
        'm' => "materialized views",
        'c' => "composite types",
        'f' => "foreign tables",
        'p' => "partitioned tables",
        'I' => "partitioned indexes",
        _ => "this kind of relation",
    }
}

/// Whether ALTER TABLE subcommand `t` applies to a foreign table (ATSimplePermissions'
/// ATT_FOREIGN_TABLE), and whether it applies to a plain or partitioned table.
fn allowed(t: pb::AlterTableType, foreign: bool) -> bool {
    use pb::AlterTableType as A;
    let foreign_only = matches!(t, A::AtAlterColumnGenericOptions | A::AtGenericOptions);
    if !foreign {
        return !foreign_only;
    }
    foreign_only
        || matches!(
            t,
            A::AtAddColumn
                | A::AtColumnDefault
                | A::AtCookedColumnDefault
                | A::AtAddIdentity
                | A::AtSetIdentity
                | A::AtDropIdentity
                | A::AtDropNotNull
                | A::AtSetNotNull
                | A::AtSetExpression
                | A::AtDropExpression
                | A::AtSetStatistics
                | A::AtSetOptions
                | A::AtResetOptions
                | A::AtSetStorage
                | A::AtDropColumn
                | A::AtAddConstraint
                | A::AtReAddConstraint
                | A::AtDropConstraint
                | A::AtValidateConstraint
                | A::AtAlterColumnType
                | A::AtChangeOwner
                | A::AtDropOids
                | A::AtAddInherit
                | A::AtDropInherit
                | A::AtEnableTrig
                | A::AtEnableAlwaysTrig
                | A::AtEnableReplicaTrig
                | A::AtDisableTrig
                | A::AtEnableTrigAll
                | A::AtDisableTrigAll
                | A::AtEnableTrigUser
                | A::AtDisableTrigUser
        )
}

impl Ddl<'_, '_> {
    /// ATSimplePermissions for what differs between foreign and other tables: subcommands a
    /// foreign table doesn't take, and OPTIONS on a table that isn't foreign.
    pub(super) fn foreign_alter_check(&self, t: &Table, cmd: &pb::AlterTableCmd) -> Result<()> {
        let sub = pb::AlterTableType::try_from(cmd.subtype).unwrap_or(pb::AlterTableType::Undefined);
        // (DETACH ... FINALIZE: ATT_PARTITIONED_TABLE only.)
        let parted_only = sub == pb::AlterTableType::AtDetachPartitionFinalize;
        if allowed(sub, t.is_foreign()) && !(parted_only && t.partkey.is_none()) {
            return Ok(());
        }
        let Some(action) = alter_action(sub) else {
            return Ok(());
        };
        Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("ALTER action {action} cannot be performed on relation \"{}\"", t.name),
        )
        .detail(format!(
            "This operation is not supported for {}.",
            relkind_plural(t.relkind())
        )))
    }

    /// The validator of the wrapper of foreign table `t`'s server.
    fn foreign_validator(&self, t: &Table) -> u32 {
        t.foreign
            .as_ref()
            .and_then(|f| self.cat.servers.get(&f.server))
            .and_then(|s| self.cat.fdws.get(&s.fdw))
            .map_or(0, |f| f.validator)
    }

    /// CREATE FOREIGN TABLE: DefineRelation as for a table, then CreateForeignTable (the
    /// server, its USAGE privilege and the options), then the columns' options.
    pub(super) fn create_foreign_table(&mut self, s: &pb::CreateForeignTableStmt) -> Result<String> {
        const TAG: &str = "CREATE FOREIGN TABLE";
        let Some(base) = s.base_stmt.as_ref() else {
            return Err(crate::error::internal("CREATE FOREIGN TABLE without a table"));
        };
        foreign_elements_check(&base.table_elts)?;
        self.creating_foreign = Some(ForeignTableDef::default());
        self.create_table(base)?;
        if self.creating_foreign.take().is_some() {
            // IF NOT EXISTS: it was there.
            return Ok(TAG.into());
        }
        let rv = base.relation.as_ref().unwrap();
        let Some(crate::catalog::Relation::Table(t)) = self.lookup(rv)? else {
            return Err(crate::error::internal("foreign table vanished"));
        };
        let srv = self.server_named(&s.servername)?;
        let (p, _) = self.cat.obj_mask(
            crate::catalog::acl::ObjKind::ForeignServer,
            srv.oid,
            self.me(),
            self.db_owner(),
        );
        if p & crate::catalog::acl::USAGE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for foreign server {}", srv.name),
            ));
        }
        let validator = self.cat.fdws.get(&srv.fdw).map_or(0, |f| f.validator);
        let options = self.generic_options(FOREIGN_TABLE_CATALOG, None, &s.options, validator)?;
        let mut def = ForeignTableDef {
            server: srv.oid,
            options,
            col_options: Vec::new(),
        };
        for e in &base.table_elts {
            if let Some(NodeEnum::ColumnDef(cd)) = inner(e)
                && !cd.fdwoptions.is_empty()
                && let Some(i) = t.column_index(&cd.colname)
            {
                let o = self.generic_options(ATTRIBUTE_CATALOG, None, &cd.fdwoptions, validator)?;
                def.set_column_options(i, o);
            }
        }
        let mut t = (*t).clone();
        t.foreign = Some(def);
        self.cat.put_table(t);
        self.save()?;
        Ok(TAG.into())
    }

    /// ALTER FOREIGN TABLE ... OPTIONS (ATExecGenericOptions).
    pub(super) fn foreign_table_options(&self, t: &mut Table, opts: &[pb::Node]) -> Result<()> {
        let validator = self.foreign_validator(t);
        let Some(f) = t.foreign.as_mut() else {
            return Ok(());
        };
        if opts.is_empty() {
            return Ok(());
        }
        f.options = self.generic_options(FOREIGN_TABLE_CATALOG, f.options.as_ref(), opts, validator)?;
        Ok(())
    }

    /// ALTER FOREIGN TABLE ... ALTER COLUMN ... OPTIONS (ATExecAlterColumnGenericOptions).
    pub(super) fn foreign_column_options(
        &self,
        t: &mut Table,
        col: &str,
        opts: &[pb::Node],
    ) -> Result<()> {
        if opts.is_empty() {
            return Ok(());
        }
        let validator = self.foreign_validator(t);
        let Some(i) = t.column_index(col) else {
            if crate::exec::scan::SYS_COLS.iter().any(|(n, _)| *n == col) {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("cannot alter system column \"{col}\""),
                ));
            }
            return Err(Error::new(
                code::UNDEFINED_COLUMN,
                format!("column \"{col}\" of relation \"{}\" does not exist", t.name),
            ));
        };
        let Some(f) = t.foreign.as_mut() else {
            return Ok(());
        };
        let o = self.generic_options(ATTRIBUTE_CATALOG, f.column_options(i), opts, validator)?;
        f.set_column_options(i, o);
        Ok(())
    }

    /// IMPORT FOREIGN SCHEMA: the server, its USAGE privilege and the target schema are
    /// checked, then the wrapper's handler would list the remote tables.
    pub(super) fn import_foreign_schema(&mut self, s: &pb::ImportForeignSchemaStmt) -> Result<String> {
        let srv = self.server_named(&s.server_name)?;
        let (p, _) = self.cat.obj_mask(
            crate::catalog::acl::ObjKind::ForeignServer,
            srv.oid,
            self.me(),
            self.db_owner(),
        );
        if p & crate::catalog::acl::USAGE == 0 {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for foreign server {}", srv.name),
            ));
        }
        let schema = self.cat.schema_oid(&s.local_schema)?;
        self.require_schema_create(schema)?;
        let fdw = self.cat.fdws.get(&srv.fdw).cloned();
        let name = fdw.as_ref().map_or(String::new(), |f| f.name.clone());
        let pg_fdw = fdw.as_ref().is_some_and(|w| {
            self.cat.functions.get(&w.handler).is_some_and(|h| {
                h.probin.as_deref().is_some_and(|b| crate::ext::native::library(b) == "postgres_fdw")
            })
        });
        if pg_fdw {
            return self.import_postgres_fdw(s, &srv);
        }
        Err(match fdw {
            Some(f) if f.handler != 0 => Error::new(
                FDW_NO_SCHEMAS,
                format!("foreign-data wrapper \"{name}\" does not support IMPORT FOREIGN SCHEMA"),
            ),
            _ => no_handler(&name),
        })
    }
}

impl Ddl<'_, '_> {
    /// ImportForeignSchema with postgres_fdw's commands: each table the LIMIT TO / EXCEPT
    /// list lets through is created in the local schema.
    fn import_postgres_fdw(&mut self, s: &pb::ImportForeignSchemaStmt, srv: &crate::catalog::userobj::ForeignServer) -> Result<String> {
        let names: Vec<String> = s
            .table_list
            .iter()
            .filter_map(|n| match inner(n) {
                Some(NodeEnum::RangeVar(r)) => Some(r.relname.clone()),
                _ => None,
            })
            .collect();
        let list = match pb::ImportForeignSchemaType::try_from(s.list_type) {
            Ok(pb::ImportForeignSchemaType::FdwImportSchemaLimitTo) => Some((true, names)),
            Ok(pb::ImportForeignSchemaType::FdwImportSchemaExcept) => Some((false, names)),
            _ => None,
        };
        let options: Vec<(String, String)> = super::def_elems(&s.options)
            .map(|d| (d.defname.clone(), crate::sql::binder::ast::def_arg_str(d).unwrap_or_default()))
            .collect();
        let imp = crate::ext::postgres_fdw::import::Import { remote_schema: &s.remote_schema, list: list.clone(), options };
        let cmds = crate::ext::postgres_fdw::import::commands(self.ctx, srv, &imp)?;
        for (table, sql) in cmds {
            // IsImportableForeignTable.
            if let Some((limit, l)) = &list
                && l.contains(&table) != *limit
            {
                continue;
            }
            let stmts = crate::sql::parse::parse(&sql)?;
            let Some(NodeEnum::CreateForeignTableStmt(mut cs)) = stmts.first().and_then(|r| r.stmt.as_deref()).and_then(|n| n.node.clone()) else {
                continue;
            };
            if let Some(base) = cs.base_stmt.as_mut()
                && let Some(rv) = base.relation.as_mut()
            {
                rv.schemaname = s.local_schema.clone();
            }
            let node = NodeEnum::CreateForeignTableStmt(cs);
            let ctx = self.ctx;
            ctx.sess.framed(crate::error::Frame::Text(format!("importing foreign table \"{table}\"")), |_| {
                // (import_error_callback: a position is one in the command's own text.)
                super::run_nested(|| super::execute(ctx, &node, &sql)).map(|_| ()).map_err(|e| match e.position {
                    Some(p) => e.internal_query(p, &sql),
                    None => e,
                })
            })?;
            self.reload_catalog();
        }
        Ok("IMPORT FOREIGN SCHEMA".into())
    }
}

/// GetFdwRoutineByServerId's error for a wrapper without a handler.
fn no_handler(fdw: &str) -> Error {
    Error::new(
        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("foreign-data wrapper \"{fdw}\" has no handler"),
    )
}
