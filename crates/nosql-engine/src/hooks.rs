//! Extension points for the subsystems built on the core (each in its own module, so parallel
//! work doesn't collide):
//!
//! | hook | who implements it |
//! |---|---|
//! | [`StatementHandler`] | statements the core doesn't execute: auth (`GRANT`, roles, users), `DESCRIBE`, service levels, index / view / function / trigger DDL, … |
//! | [`Authorizer`] | auth: permission checks before execution |
//! | [`Conditional`] | LWT: modifications with `IF` conditions |
//! | [`IndexPlanner`] | secondary indexes / SAI / vector: queries an index can answer |
//! | [`WriteObserver`] | derived data: materialized views, index maintenance, CDC |
//!
//! Register an implementation in [`install`]; the database calls it at open. Hooks see the
//! database through [`crate::Database`]'s public API and statements as parsed (`'static` ASTs).

use std::sync::Arc;

use futures::future::BoxFuture;
use nosql_cql::ast::{Statement, StatementKind};
use nosql_storage::PartitionUpdate;
use nosql_storage::manifest::TableId;

use crate::Database;
use crate::config::DbConfig;
use crate::error::{Error, Result};
use crate::response::{QueryParams, Response};
use crate::schema::Table;
use crate::session::SessionState;

/// Executes statements the core leaves to another subsystem.
pub trait StatementHandler: Send + Sync {
    /// Whether this handler executes `stmt`.
    fn handles(&self, stmt: &StatementKind<'_>) -> bool;

    /// Executes a statement this handler [`handles`](Self::handles).
    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a mut SessionState,
        stmt: &'a Statement<'static>,
        params: &'a QueryParams,
    ) -> BoxFuture<'a, Result<Response>>;
}

/// What a statement does, for permission checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access<'a> {
    Select {
        keyspace: &'a str,
        table: &'a str,
    },
    Modify {
        keyspace: &'a str,
        table: &'a str,
    },
    /// DDL on a keyspace (`table` is empty for keyspace-level changes).
    Schema {
        keyspace: &'a str,
        table: &'a str,
        statement: &'static str,
    },
    Truncate {
        keyspace: &'a str,
        table: &'a str,
    },
    Use {
        keyspace: &'a str,
    },
    /// A permission on any resource (functions, roles, …) for statements the variants above
    /// don't describe (see `crate::auth::authorizer` for every access's permission).
    Permission {
        permission: nosql_cql::ast::Permission,
        resource: crate::auth::Resource,
    },
}

impl Access<'_> {
    /// Whether the access only reads: allowed on read-only databases ([`ReadOnly`]).
    pub fn reads_only(&self) -> bool {
        use nosql_cql::ast::Permission as P;
        match self {
            Access::Select { .. } | Access::Use { .. } => true,
            Access::Permission { permission, .. } => matches!(
                permission,
                P::Select | P::Describe | P::Execute | P::Unmask | P::SelectMasked
            ),
            Access::Modify { .. } | Access::Schema { .. } | Access::Truncate { .. } => false,
        }
    }
}

/// Whether a statement only reads (`SELECT`, `USE`, `LIST ROLES / USERS / PERMISSIONS / SERVICE
/// LEVELS`, `DESCRIBE`): allowed on read-only databases ([`ReadOnly`]).
pub fn reads_only(stmt: &StatementKind<'_>) -> bool {
    use StatementKind as S;
    matches!(
        stmt,
        S::Select(_)
            | S::Use(_)
            | S::ListPermissions(_)
            | S::ListRoles(_)
            | S::ListUsers
            | S::ListServiceLevels(_)
            | S::Describe(_)
    )
}

/// Checks permissions (default: everything is allowed, as `AllowAllAuthorizer`).
pub trait Authorizer: Send + Sync {
    fn authorize(&self, db: &Database, session: &SessionState, access: &Access) -> Result<()>;

    /// Before a statement a [`StatementHandler`] runs, or core DDL: role, permission and
    /// service level statements, `DESCRIBE`, index / view / function DDL, … (the handlers check
    /// their own permissions with [`Authorizer::authorize`]; this sees the statement itself).
    fn authorize_statement(
        &self,
        _db: &Database,
        _session: &SessionState,
        _stmt: &StatementKind<'_>,
    ) -> Result<()> {
        Ok(())
    }

    /// After a statement changed the schema (`before` is the schema it ran on): grants the
    /// creator permissions on what it created, revokes permissions on what it dropped.
    fn schema_changed<'a>(
        &'a self,
        _db: &'a Arc<Database>,
        _session: &'a SessionState,
        _before: &'a crate::schema::Schema,
        _change: &'a crate::response::SchemaChange,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Wraps a database's authorizer (at open and in `Database::set_hooks`): while the database is
/// read-only ([`Database::set_read_only`]), only reads pass ([`Access::reads_only`],
/// [`reads_only`]); everything else fails with
/// [`crate::messages::read_only`] before the wrapped authorizer runs.
pub struct ReadOnly(pub Arc<dyn Authorizer>);

impl ReadOnly {
    pub fn wrap(inner: Arc<dyn Authorizer>) -> Arc<dyn Authorizer> {
        Arc::new(ReadOnly(inner))
    }
}

impl Authorizer for ReadOnly {
    fn authorize(&self, db: &Database, session: &SessionState, access: &Access) -> Result<()> {
        if db.is_read_only() && !access.reads_only() {
            return Err(crate::messages::read_only());
        }
        self.0.authorize(db, session, access)
    }

    fn authorize_statement(
        &self,
        db: &Database,
        session: &SessionState,
        stmt: &StatementKind<'_>,
    ) -> Result<()> {
        if db.is_read_only() && !reads_only(stmt) {
            return Err(crate::messages::read_only());
        }
        self.0.authorize_statement(db, session, stmt)
    }

    fn schema_changed<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a SessionState,
        before: &'a crate::schema::Schema,
        change: &'a crate::response::SchemaChange,
    ) -> BoxFuture<'a, Result<()>> {
        self.0.schema_changed(db, session, before, change)
    }
}

/// The default authorizer.
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _: &Database, _: &SessionState, _: &Access) -> Result<()> {
        Ok(())
    }
}

/// A conditional modification (`IF …`) or a batch with at least one, ready to run: the statements
/// as prepared by the core plus the values bound for this execution.
pub struct ConditionalRequest<'a> {
    pub table: &'a Arc<Table>,
    /// The CQL statement (`None` for a protocol BATCH).
    pub statement: Option<&'a Statement<'static>>,
    pub params: &'a QueryParams,
    /// Every statement (one, or a batch's members) with its bound values; each plan carries
    /// its typed `IF` clause ([`crate::statements::modify::Conditions`]).
    pub members: Vec<(
        &'a crate::statements::modify::ModifyPlan,
        &'a [crate::response::Param],
    )>,
    /// A BATCH (result rows carry the primary key columns).
    pub batch: bool,
}

/// Lightweight transactions.
pub trait Conditional: Send + Sync {
    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        request: ConditionalRequest<'a>,
    ) -> BoxFuture<'a, Result<Response>>;
}

/// Secondary-index query planning (`Index.searcherFor`, `StatementRestrictions` index checks).
pub trait IndexPlanner: Send + Sync {
    /// Whether an index on `table` supports this restriction on column `column` (a table column
    /// index): `Index.supportsExpression`. The core then lets the restriction through without
    /// `ALLOW FILTERING`, as Cassandra's `hasQueriableIndex`.
    fn supports(
        &self,
        table: &Table,
        column: usize,
        pred: &crate::prepare::restrictions::Pred,
    ) -> bool;

    /// `StatementRestrictions.needFiltering` for a query that uses an index: whether no index
    /// group serves every restriction handed to the row filter (`pk` / `ck`: whether the
    /// partition key / clustering restrictions are part of it).
    fn needs_filtering(
        &self,
        table: &Table,
        r: &crate::prepare::restrictions::Restrictions,
        pk: bool,
        ck: bool,
    ) -> bool;

    /// The index read of a SELECT whose restrictions use an index (prepare time). `None`: the
    /// core scans and filters (the result is the same, in token order).
    fn plan(
        &self,
        db: &Arc<Database>,
        table: &Arc<Table>,
        r: &crate::prepare::restrictions::Restrictions,
        ann: Option<&crate::statements::select::AnnOrder>,
    ) -> Result<Option<Arc<dyn IndexScan>>>;

    /// The index ScyllaDB's trace says a query consults ("Consulting index …"): the first
    /// restriction in WHERE order an index serves by equality, the partition's own included.
    fn consulted(
        &self,
        _table: &Table,
        _r: &crate::prepare::restrictions::Restrictions,
    ) -> Option<String> {
        None
    }
}

/// An index read: base partitions in result order (the core applies the row filter, the
/// selection, limits and paging on them).
pub trait IndexScan: Send + Sync {
    /// Opens the read, resuming after the paging state's position. `page`: the page size.
    fn open(
        &self,
        db: &Arc<Database>,
        plan: &crate::statements::select::SelectPlan,
        cx: &crate::prepare::terms::EvalCtx<'_>,
        after: Option<&crate::statements::paging::PagingState>,
        page: Option<u64>,
    ) -> Result<IndexRead>;

    /// Adjusts the query's bound row filter (an index that evaluates a restriction itself, e.g.
    /// with an analyzer, removes or replaces it); fails on restrictions the index can't serve
    /// once their values are known (SASI's `LIKE` forms).
    fn adjust_filter(&self, _filter: &mut crate::prepare::restrictions::RowFilter) -> Result<()> {
        Ok(())
    }

    /// The result is computed whole (ANN ordering): paged by row count, not by position.
    fn computed(&self) -> bool {
        false
    }
}

/// The partitions an [`IndexScan`] reads.
pub struct IndexRead {
    pub chunks: futures::stream::BoxStream<'static, Result<nosql_storage::read::PartitionChunk>>,
    /// Paged by index entries (ScyllaDB reads `page` index entries per page, however many rows
    /// they yield): once `chunks` ends, where the next page resumes (`None`: the index is
    /// exhausted). `None` here: the core pages by rows.
    pub next: Option<Arc<parking_lot::Mutex<Option<bytes::Bytes>>>>,
}

impl IndexRead {
    pub fn rows(chunks: futures::stream::BoxStream<'static, Result<nosql_storage::read::PartitionChunk>>) -> Self {
        IndexRead { chunks, next: None }
    }
}

/// Derived updates of one write, plus the locks that keep them consistent until applied.
#[derive(Default)]
pub struct Derived {
    pub updates: Vec<(TableId, PartitionUpdate)>,
    /// Held until the write and `updates` are applied.
    pub guards: Vec<Box<dyn std::any::Any + Send>>,
}

/// Observes every write before it is applied (views, index tables, CDC log). Returns extra
/// updates to apply atomically with the write.
pub trait WriteObserver: Send + Sync {
    /// `writes`: every update of one atomic write (a statement or a whole batch) with its table.
    fn on_write<'a>(
        &'a self,
        db: &'a Arc<Database>,
        writes: &'a [(Arc<Table>, PartitionUpdate)],
    ) -> BoxFuture<'a, Result<Derived>>;

    /// Waits until every write that started before the call is applied (a new view's backfill
    /// starts after it, so no write is missed by both the backfill and the maintenance).
    fn barrier<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// The hooks of one database.
#[derive(Clone)]
pub struct Hooks {
    pub statements: Vec<Arc<dyn StatementHandler>>,
    pub authorizer: Arc<dyn Authorizer>,
    pub conditional: Option<Arc<dyn Conditional>>,
    pub index: Option<Arc<dyn IndexPlanner>>,
    pub observers: Vec<Arc<dyn WriteObserver>>,
}

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            statements: Vec::new(),
            authorizer: Arc::new(AllowAll),
            conditional: None,
            index: None,
            observers: Vec::new(),
        }
    }
}

impl Hooks {
    pub fn handler_for(&self, stmt: &StatementKind<'_>) -> Option<Arc<dyn StatementHandler>> {
        self.statements.iter().find(|h| h.handles(stmt)).cloned()
    }
}

/// Installs every subsystem's hooks. Each subsystem adds one line here.
pub fn install(hooks: &mut Hooks, cfg: &DbConfig) {
    crate::lwt::install(hooks, cfg);
    crate::auth::install(hooks, cfg);
    crate::index::install(hooks, cfg);
    crate::views::install(hooks, cfg);
    crate::describe::install(hooks, cfg);
    crate::udf::install(hooks, cfg);
}

/// The error for a statement no subsystem implements yet.
pub fn unsupported(stmt: &StatementKind<'_>) -> Error {
    let name = format!("{stmt:?}");
    let kind = name.split('(').next().unwrap_or("statement");
    Error::invalid(format!("{kind} statements are not supported yet"))
}
