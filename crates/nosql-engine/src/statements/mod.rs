//! Prepared statements and their execution.
//!
//! [`prepare`] turns a parsed statement into a [`Prepared`] plan against a schema snapshot;
//! [`execute`] runs it with bound values. Plans record the tables (and versions) they depend on:
//! after a schema change touching one of them the plan is stale and EXECUTE answers UNPREPARED,
//! so drivers re-prepare (Cassandra's `QueryProcessor` evicts on schema change the same way).
//!
//! # Adding a statement
//!
//! 1. Give it a [`Plan`] variant (or route it to a hook in `crate::hooks` when it belongs to
//!    another subsystem).
//! 2. Prepare it in [`prepare`]: resolve names, type terms (`crate::prepare::terms`), record bind
//!    specs; fail with the reference's message.
//! 3. Execute it in [`execute`], checking access through the authorizer first.

pub mod batch;
pub mod modify;
pub mod paging;
pub(crate) mod read_warn;
pub mod select;

use std::sync::Arc;

use md5::{Digest, Md5};
use nosql_cql::ast::{Statement, StatementKind};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::Access;
use crate::messages as msg;
use crate::response::{ColumnSpec, QueryParams, Response, ResponseBody, ResultMetadata};
use crate::schema::{Schema, Table};
use crate::session::SessionState;

/// What a prepared statement does.
#[derive(Clone)]
pub enum Plan {
    Select(Arc<select::SelectPlan>),
    Modify(Arc<modify::ModifyPlan>),
    Batch(Arc<batch::BatchPlan>),
    Use(String),
    Truncate {
        keyspace: Option<String>,
        table: String,
    },
    /// Schema changes run against the schema current at execution.
    Ddl,
    /// Executed by a subsystem hook (or rejected when none handles it).
    Other,
}

/// A prepared statement.
pub struct Prepared {
    pub id: [u8; 16],
    pub text: String,
    /// The keyspace unqualified names resolved against.
    pub keyspace: Option<String>,
    pub stmt: Arc<Statement<'static>>,
    pub plan: Plan,
    pub bind: Vec<ColumnSpec>,
    pub pk_indexes: Vec<u16>,
    /// `None`: the statement returns no rows (or its columns are only known at execution).
    pub result: Option<ResultMetadata>,
    /// `(table id, version)` of every table the plan depends on.
    pub deps: Vec<([u8; 16], [u8; 16])>,
    pub is_lwt: bool,
}

impl Prepared {
    /// Whether every table the plan was prepared on still has the same version.
    pub fn is_valid(&self, schema: &Schema) -> bool {
        self.deps.iter().all(|(id, version)| {
            schema
                .table_by_id(id)
                .is_some_and(|t| &t.version == version)
        })
    }
}

/// `MD5(keyspace + query)` (`QueryProcessor.computeId`).
pub fn statement_id(text: &str, keyspace: Option<&str>) -> [u8; 16] {
    let mut h = Md5::new();
    if let Some(k) = keyspace {
        h.update(k.as_bytes());
    }
    h.update(text.as_bytes());
    let d = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&d[..16]);
    out
}

/// Resolves `[keyspace.]table` for a data statement.
pub fn resolve_table(
    db: &Database,
    schema: &Schema,
    name: &nosql_cql::ast::QualifiedName,
    session_ks: Option<&str>,
) -> Result<Arc<Table>> {
    let d = db.dialect();
    let ks = match &name.keyspace {
        Some(k) => k.name().to_string(),
        None => session_ks
            .map(str::to_string)
            .ok_or_else(msg::no_keyspace)?,
    };
    let Some(keyspace) = schema.keyspace(&ks) else {
        return Err(msg::keyspace_does_not_exist(d, &ks));
    };
    let tname = name.name.name();
    keyspace
        .tables
        .get(tname.as_ref())
        .cloned()
        .ok_or_else(|| msg::unconfigured_table(d, &tname))
}

/// Prepares a parsed statement against `schema`.
pub fn prepare(
    db: &Arc<Database>,
    schema: &Arc<Schema>,
    stmt: Statement<'static>,
    text: &str,
    keyspace: Option<&str>,
) -> Result<Prepared> {
    if stmt.bind_markers.len() > crate::limits::MAX_MARKERS {
        return Err(Error::invalid(format!(
            "Too many markers(?). {} markers exceed the allowed maximum of {}",
            stmt.bind_markers.len(),
            crate::limits::MAX_MARKERS
        )));
    }
    // ScyllaDB's `max_relations_in_where_clause`.
    if db.dialect().is_scylla() {
        let relations = match &stmt.kind {
            StatementKind::Select(s) => s.where_clause.len(),
            StatementKind::Update(u) => u.where_clause.len(),
            StatementKind::Delete(x) => x.where_clause.len(),
            _ => 0,
        };
        if relations > crate::limits::SCYLLA_MAX_RELATIONS {
            return Err(Error::syntax("too many relations in WHERE clause"));
        }
    }
    let id = statement_id(text, keyspace);
    let stmt = Arc::new(stmt);
    let mut prepared = Prepared {
        id,
        text: text.to_string(),
        keyspace: keyspace.map(str::to_string),
        stmt: stmt.clone(),
        plan: Plan::Other,
        bind: Vec::new(),
        pk_indexes: Vec::new(),
        result: None,
        deps: Vec::new(),
        is_lwt: false,
    };
    match &stmt.kind {
        StatementKind::Select(s) => {
            let plan = select::prepare(db, schema, s, &stmt, keyspace)?;
            prepared.bind = plan.bind.clone();
            prepared.pk_indexes = plan.pk_indexes.clone();
            prepared.result = Some(ResultMetadata {
                columns: plan.result_specs(),
            });
            prepared.deps = vec![(plan.table.id, plan.table.version)];
            prepared.plan = Plan::Select(Arc::new(plan));
        }
        StatementKind::Insert(_) | StatementKind::Update(_) | StatementKind::Delete(_) => {
            let plan = modify::prepare(db, schema, &stmt.kind, &stmt, keyspace)?;
            prepared.bind = plan.bind.clone();
            prepared.pk_indexes = plan.pk_indexes.clone();
            prepared.deps = vec![(plan.table.id, plan.table.version)];
            prepared.is_lwt = plan.conditional;
            // ScyllaDB announces the result columns of a conditional statement at PREPARE.
            prepared.result = crate::lwt::prepared_result(db.dialect(), &[&plan], false);
            prepared.plan = Plan::Modify(Arc::new(plan));
        }
        StatementKind::Batch(b) => {
            let plan = batch::prepare(db, schema, b, &stmt, keyspace)?;
            prepared.bind = plan.bind.clone();
            prepared.pk_indexes = plan.pk_indexes.clone();
            prepared.deps = plan
                .statements
                .iter()
                .map(|s| (s.table.id, s.table.version))
                .collect();
            prepared.is_lwt = plan.conditional;
            let members: Vec<&modify::ModifyPlan> =
                plan.statements.iter().map(|s| s.as_ref()).collect();
            prepared.result = crate::lwt::prepared_result(db.dialect(), &members, true);
            prepared.plan = Plan::Batch(Arc::new(plan));
        }
        StatementKind::Use(ident) => prepared.plan = Plan::Use(ident.name().to_string()),
        StatementKind::Truncate(t) => {
            prepared.plan = Plan::Truncate {
                keyspace: t.table.keyspace.as_ref().map(|k| k.name().to_string()),
                table: t.table.name.name().to_string(),
            };
            crate::schema::ddl::check_timeout(&t.using, db.dialect())?;
        }
        StatementKind::CreateTable(c) => {
            crate::schema::ddl::validate_create_options(db.dialect(), c)?;
            prepared.plan = Plan::Ddl
        }
        StatementKind::CreateKeyspace(_)
        | StatementKind::AlterKeyspace(_)
        | StatementKind::DropKeyspace(_)
        | StatementKind::AlterTable(_)
        | StatementKind::DropTable(_)
        | StatementKind::CreateType(_)
        | StatementKind::AlterType(_)
        | StatementKind::DropType(_) => {
            // ScyllaDB's `DROP c USING TIMESTAMP ?`.
            if let StatementKind::AlterTable(a) = &stmt.kind
                && let nosql_cql::ast::AlterTableAction::DropObject {
                    timestamp: Some(nosql_cql::ast::Term::Bind(_)),
                    ..
                } = &a.action
            {
                let ks = a
                    .name
                    .keyspace
                    .as_ref()
                    .map(|k| k.name().to_string())
                    .or_else(|| keyspace.map(str::to_string))
                    .unwrap_or_default();
                prepared.bind = vec![ColumnSpec::new(
                    &ks,
                    a.name.name.name().as_ref(),
                    "[timestamp]",
                    nosql_types::CqlType::Bigint,
                )];
            }
            prepared.plan = Plan::Ddl
        }
        _ => prepared.plan = Plan::Other,
    }
    Ok(prepared)
}

/// Executes a prepared statement with ordered values.
pub async fn execute(
    db: &Arc<Database>,
    session: &mut SessionState,
    p: &Prepared,
    params: &QueryParams,
) -> Result<Response> {
    let hooks = db.hooks();
    match &p.plan {
        Plan::Select(plan) => {
            hooks.authorizer.authorize(
                db,
                session,
                &Access::Select {
                    keyspace: &plan.table.keyspace,
                    table: &plan.table.name,
                },
            )?;
            authorize_functions(db, session, &plan.functions)?;
            crate::masking::check_restrictions(
                db,
                session,
                &plan.table,
                &select::restricted_columns(plan),
            )?;
            let masks = crate::masking::read_masks(db, session, &plan.table)?;
            select::execute_masked(db, plan, params, masks).await
        }
        Plan::Modify(plan) => {
            authorize_modify(db, session, plan)?;
            modify::execute(db, plan, params, &p.stmt).await
        }
        Plan::Batch(plan) => {
            for s in &plan.statements {
                authorize_modify(db, session, s)?;
            }
            batch::execute(db, plan, params, &p.stmt).await
        }
        Plan::Use(ks) => {
            hooks
                .authorizer
                .authorize(db, session, &Access::Use { keyspace: ks })?;
            if db.schema().keyspace(ks).is_none() {
                return Err(msg::use_unknown_keyspace(ks));
            }
            session.keyspace = Some(ks.clone());
            Ok(Response::new(ResponseBody::SetKeyspace(ks.clone())))
        }
        Plan::Truncate { keyspace, table } => {
            let ks = keyspace
                .clone()
                .or_else(|| p.keyspace.clone())
                .or_else(|| session.keyspace.clone())
                .ok_or_else(msg::no_keyspace)?;
            hooks.authorizer.authorize(
                db,
                session,
                &Access::Truncate {
                    keyspace: &ks,
                    table,
                },
            )?;
            match &p.stmt.kind {
                StatementKind::Truncate(t) => {
                    crate::schema::ddl::truncate_using(db, &ks, table, &t.using, &params.values)
                        .await
                }
                _ => crate::schema::ddl::truncate(db, &ks, table).await,
            }
        }
        Plan::Ddl => {
            hooks
                .authorizer
                .authorize_statement(db, session, &p.stmt.kind)?;
            let before = db.schema();
            let r = if let Some(h) = hooks.handler_for(&p.stmt.kind) {
                h.execute(db, session, &p.stmt, params).await
            } else {
                crate::schema::ddl::execute(
                    db,
                    session,
                    &p.stmt,
                    p.keyspace.as_deref(),
                    &params.values,
                )
                .await
            };
            after_schema_change(db, session, &before, r).await
        }
        Plan::Other => {
            hooks
                .authorizer
                .authorize_statement(db, session, &p.stmt.kind)?;
            let before = db.schema();
            let r = match hooks.handler_for(&p.stmt.kind) {
                Some(h) => h.execute(db, session, &p.stmt, params).await,
                None => {
                    let ks = p.keyspace.as_deref().or(session.keyspace.as_deref());
                    missing_object(db, &p.stmt.kind, ks)
                        .unwrap_or_else(|| Err(crate::hooks::unsupported(&p.stmt.kind)))
                }
            };
            after_schema_change(db, session, &before, r).await
        }
    }
}

/// MODIFY on the table, and SELECT too for a conditional statement (Cassandra's
/// `ModificationStatement.authorize`: an `IF` can read the row).
pub(crate) fn authorize_modify(
    db: &Database,
    session: &SessionState,
    plan: &modify::ModifyPlan,
) -> Result<()> {
    let hooks = db.hooks();
    let (keyspace, table) = (plan.table.keyspace.as_str(), plan.table.name.as_str());
    hooks
        .authorizer
        .authorize(db, session, &Access::Modify { keyspace, table })?;
    if plan.conditional {
        hooks
            .authorizer
            .authorize(db, session, &Access::Select { keyspace, table })?;
    }
    authorize_functions(db, session, &plan.functions)
}

/// EXECUTE on every user function a statement calls (`SelectStatement.authorize`,
/// `ModificationStatement.authorize`).
pub(crate) fn authorize_functions(
    db: &Database,
    session: &SessionState,
    functions: &[Arc<dyn crate::functions::Function>],
) -> Result<()> {
    if functions.is_empty() {
        return Ok(());
    }
    let hooks = db.hooks();
    for f in functions {
        let n = f.name();
        hooks.authorizer.authorize(
            db,
            session,
            &Access::Permission {
                permission: nosql_cql::ast::Permission::Execute,
                resource: crate::udf::ddl::resource(&n.keyspace, &n.name, f.arg_types(), db.dialect()),
            },
        )?;
    }
    Ok(())
}

/// Lets the authorizer react to a schema change (creator permissions, cleanup on drop).
async fn after_schema_change(
    db: &Arc<Database>,
    session: &SessionState,
    before: &Schema,
    r: Result<Response>,
) -> Result<Response> {
    let r = r?;
    if let ResponseBody::SchemaChange(c) = &r.body {
        let authorizer = db.hooks().authorizer;
        authorizer.schema_changed(db, session, before, c).await?;
    }
    Ok(r)
}

/// `DROP INDEX` / `DROP MATERIALIZED VIEW` of an object that doesn't exist, answered without the
/// index and view subsystems (which own every other case).
fn missing_object(
    db: &Database,
    kind: &StatementKind<'static>,
    current: Option<&str>,
) -> Option<Result<Response>> {
    let d = db.dialect();
    let (drop, is_index) = match kind {
        StatementKind::DropIndex(x) => (x, true),
        StatementKind::DropView(x) => (x, false),
        _ => return None,
    };
    let name = drop.name.name.name().to_string();
    let ks = match drop
        .name
        .keyspace
        .as_ref()
        .map(|k| k.name().to_string())
        .or_else(|| current.map(str::to_string))
    {
        Some(k) => k,
        None => return Some(Err(msg::no_keyspace())),
    };
    let schema = db.schema();
    let exists = schema.keyspace(&ks).is_some_and(|k| {
        if is_index {
            k.tables
                .values()
                .any(|t| t.indexes.iter().any(|i| i.name == name))
        } else {
            k.tables.get(&name).is_some_and(|t| t.view.is_some())
        }
    });
    if exists {
        return None;
    }
    if drop.if_exists {
        return Some(Ok(Response::void()));
    }
    Some(Err(match (d, is_index) {
        (nosql_types::Dialect::Scylla, true) => Error::invalid(format!(
            "Index '{name}' could not be found in any of the tables of keyspace '{ks}'"
        )),
        (nosql_types::Dialect::Cassandra, true) => {
            Error::invalid(format!("Index '{ks}.{name}' doesn't exist'"))
        }
        (nosql_types::Dialect::Scylla, false) => Error::config(format!(
            "Cannot drop non existing materialized view '{name}' in keyspace '{ks}'."
        )),
        (nosql_types::Dialect::Cassandra, false) => {
            Error::invalid(format!("Materialized view '{ks}.{name}' doesn't exist"))
        }
    }))
}
