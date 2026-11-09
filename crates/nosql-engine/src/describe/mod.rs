//! Server-side `DESCRIBE`: the schema (and ScyllaDB's roles, grants and service levels) as CQL,
//! in rows of `keyspace_name | type | name | create_statement` (`system.describe`), byte for
//! byte as each reference prints them.
//!
//! Cassandra's statement is a port of `org.apache.cassandra.cql3.statements.DescribeStatement`
//! with the element renderers of `KeyspaceMetadata`, `TableMetadata` (incl.
//! `CompactTableMetadata`), `TableParams`, `IndexMetadata`, `ViewMetadata`, `UserType`,
//! `UDFunction` and `UDAggregate` (`toCqlString`, see [`cassandra`]). ScyllaDB's forms
//! ([`scylla`]) follow the captures in `docs/nosql/reference/describe*.md` and live probing.
//!
//! # Layout
//!
//! - [`Element`] is one described object; the functions [`schema`], [`keyspace`], [`table`],
//!   [`index`], [`view`], [`user_type`], [`function`], [`aggregate`] and [`listing`] return them,
//!   so other code (the control plane's schema diff, the dashboard) reuses exactly what
//!   `DESCRIBE` prints.
//! - [`Handler`] is the [`StatementHandler`] for `DESCRIBE` statements: it resolves the target,
//!   checks the ScyllaDB-only rules (`WITH PASSWORDS`), and pages the rows like Cassandra (an
//!   offset in a paging state tied to the schema version; ScyllaDB never pages).
//!
//! Element order: keyspaces by name (Cassandra ignoring case; ScyllaDB's own order is that of a
//! hash map, so roll uses byte order), then per keyspace the keyspace, its types in dependency
//! order, functions, aggregates, and every table followed by its indexes and views. ScyllaDB's
//! internal tables of secondary indexes (`<index>_index` views) are described only by
//! `DESCRIBE MATERIALIZED VIEW`; CDC log tables (`<table>_scylla_cdc_log`) as `ALTER TABLE`.
//!
//! User functions are rendered from the metadata in `system_schema.functions` /
//! `aggregates` ([`crate::schema::FunctionMeta`]), whatever language; the UDF subsystem only has
//! to store them there.

pub mod builder;
pub mod cassandra;
pub mod messages;
pub mod paxos;
pub mod scylla;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use nosql_cql::ast::{Describe, DescribeTarget, Statement, StatementKind};
use nosql_types::{CqlType, Dialect};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::StatementHandler;
use crate::response::{ColumnSpec, QueryParams, Response, ResponseBody, ResultMetadata, Rows};
use crate::schema::{Keyspace, KeyspaceKind, Schema, Table};
use crate::session::SessionState;
use messages as m;

/// What a `DESCRIBE` row describes (its `type` column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Keyspace,
    Type,
    Function,
    Aggregate,
    Table,
    Index,
    View,
    /// ScyllaDB `WITH INTERNALS`: roles, role grants, permission grants, service levels and
    /// their attachments.
    Role,
    GrantRole,
    GrantPermission,
    ServiceLevel,
    ServiceLevelAttachment,
}

impl Kind {
    /// The `type` column.
    pub fn name(self, d: Dialect) -> &'static str {
        match self {
            Kind::Keyspace => "keyspace",
            Kind::Type => "type",
            Kind::Function => "function",
            Kind::Aggregate => "aggregate",
            Kind::Table => "table",
            Kind::Index => "index",
            Kind::View if d.is_scylla() => "view",
            Kind::View => "materialized_view",
            Kind::Role => "role",
            Kind::GrantRole => "grant_role",
            Kind::GrantPermission => "grant_permission",
            Kind::ServiceLevel => "service_level",
            Kind::ServiceLevelAttachment => "service_level_attachment",
        }
    }
}

/// One described object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    /// `None` for roles, grants and service levels.
    pub keyspace: Option<String>,
    pub kind: Kind,
    /// The object's name, unquoted.
    pub name: String,
    /// Cassandra functions and aggregates: the argument list (`(int, int)`) appended to the
    /// name column.
    pub signature: Option<String>,
    /// `None` in listings (`DESCRIBE TABLES`, …).
    pub statement: Option<String>,
}

impl Element {
    fn new(keyspace: &str, kind: Kind, name: &str, statement: String) -> Element {
        Element {
            keyspace: Some(keyspace.to_string()),
            kind,
            name: name.to_string(),
            signature: None,
            statement: Some(statement),
        }
    }

    /// The `keyspace_name`, `type`, `name` and `create_statement` columns.
    pub fn row(&self, d: Dialect) -> Vec<Option<Bytes>> {
        let text = |s: String| Some(Bytes::from(s));
        let mut row = vec![
            self.keyspace
                .as_deref()
                .and_then(|k| text(builder::quote(k, d))),
            text(self.kind.name(d).to_string()),
            text(format!(
                "{}{}",
                builder::quote(&self.name, d),
                self.signature.as_deref().unwrap_or("")
            )),
        ];
        if let Some(s) = &self.statement {
            row.push(text(s.clone()));
        }
        row
    }
}

/// `WITH INTERNALS [AND PASSWORDS]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Table ids, dropped columns, ScyllaDB's auth objects.
    pub internals: bool,
    /// ScyllaDB: roles' password hashes.
    pub passwords: bool,
}

/// What a listing statement lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingOf {
    Tables,
    Types,
    Functions,
    Aggregates,
}

/// The `DESCRIBE` statement handler.
pub struct Handler;

impl StatementHandler for Handler {
    fn handles(&self, stmt: &StatementKind<'_>) -> bool {
        matches!(stmt, StatementKind::Describe(_))
    }

    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a mut SessionState,
        stmt: &'a Statement<'static>,
        params: &'a QueryParams,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async move {
            let StatementKind::Describe(desc) = &stmt.kind else {
                return Err(crate::hooks::unsupported(&stmt.kind));
            };
            execute(db, session, desc, params).await
        })
    }
}

/// Installs the handler.
pub fn install(hooks: &mut crate::hooks::Hooks, _cfg: &crate::config::DbConfig) {
    hooks.statements.push(Arc::new(Handler));
}

/// Runs a `DESCRIBE` statement.
pub async fn execute(
    db: &Arc<Database>,
    session: &SessionState,
    desc: &Describe<'static>,
    params: &QueryParams,
) -> Result<Response> {
    let d = db.dialect();
    let opts = Options {
        internals: desc.with_internals,
        passwords: desc.with_passwords,
    };
    if opts.passwords {
        if !matches!(desc.target, DescribeTarget::Schema { .. }) {
            return Err(m::passwords_only_with_schema());
        }
        let cx = crate::auth::authorizer::Ctx::new(db, session);
        if !cx.user_is_super() {
            return Err(m::passwords_need_superuser());
        }
    }
    if d.is_scylla() && opts.internals && matches!(desc.target, DescribeTarget::Schema { .. }) {
        crate::auth::store::ensure_fresh(db).await?;
    }
    let schema = db.schema();
    let current = params.keyspace.as_deref().or(session.keyspace.as_deref());
    let (columns, rows) = match &desc.target {
        DescribeTarget::Cluster => cluster(db, &schema, current),
        target => {
            let (elements, listing) = elements(db, &schema, target, current, opts)?;
            let names: &[&str] = if listing {
                &["keyspace_name", "type", "name"]
            } else {
                &["keyspace_name", "type", "name", "create_statement"]
            };
            let cols = names
                .iter()
                .map(|n| ColumnSpec::new("system", "describe", *n, CqlType::Text))
                .collect();
            (cols, elements.iter().map(|e| e.row(d)).collect())
        }
    };
    page(d, &schema, columns, rows, params)
}

/// The elements a target describes; `true` for a listing (no statements).
fn elements(
    db: &Database,
    schema: &Schema,
    target: &DescribeTarget<'static>,
    current: Option<&str>,
    opts: Options,
) -> Result<(Vec<Element>, bool)> {
    use DescribeTarget as T;
    let qualified = |n: &nosql_cql::ast::QualifiedName<'static>| -> Result<(String, String)> {
        let ks = match &n.keyspace {
            Some(k) => k.name().to_string(),
            None => current.map(str::to_string).ok_or_else(m::no_keyspace)?,
        };
        Ok((ks, n.name.name().to_string()))
    };
    let function_name = |n: &nosql_cql::ast::FunctionName<'static>| -> Result<(String, String)> {
        let ks = match &n.keyspace {
            Some(k) => k.name().to_string(),
            None => current.map(str::to_string).ok_or_else(m::no_keyspace)?,
        };
        Ok((ks, n.name.name().to_string()))
    };
    Ok(match target {
        T::Cluster => (Vec::new(), false),
        T::Schema { full } => (self::schema(db, schema, *full, opts), false),
        T::Keyspaces => (keyspaces(db, schema), true),
        T::Keyspace { only, name } => {
            let ks = match name {
                Some(n) => n.name().to_string(),
                None => current.map(str::to_string).ok_or_else(m::no_keyspace)?,
            };
            (keyspace(db, schema, &ks, *only, opts)?, false)
        }
        T::Tables => (listing(db, schema, ListingOf::Tables, current)?, true),
        T::Types => (listing(db, schema, ListingOf::Types, current)?, true),
        T::Functions => (listing(db, schema, ListingOf::Functions, current)?, true),
        T::Aggregates => (listing(db, schema, ListingOf::Aggregates, current)?, true),
        T::Table(n) => {
            let (ks, t) = qualified(n)?;
            (table(db, schema, &ks, &t, opts)?, false)
        }
        T::Index(n) => {
            let (ks, i) = qualified(n)?;
            (index(db, schema, &ks, &i, opts)?, false)
        }
        T::MaterializedView(n) => {
            let (ks, v) = qualified(n)?;
            (view(db, schema, &ks, &v, opts)?, false)
        }
        T::Type(n) => {
            let (ks, t) = qualified(n)?;
            (user_type(db, schema, &ks, &t)?, false)
        }
        T::Function(n) => {
            let (ks, f) = function_name(n)?;
            (function(db, schema, &ks, &f)?, false)
        }
        T::Aggregate(n) => {
            let (ks, a) = function_name(n)?;
            (aggregate(db, schema, &ks, &a)?, false)
        }
        T::Generic(n) => (generic(db, schema, n, current, opts)?, false),
    })
}

/// `DESCRIBE [ks.]name`: a keyspace, a table, an index or a view (in this order; ScyllaDB then
/// a type, a function or an aggregate).
fn generic(
    db: &Database,
    schema: &Schema,
    n: &nosql_cql::ast::QualifiedName<'static>,
    current: Option<&str>,
    opts: Options,
) -> Result<Vec<Element>> {
    let name = n.name.name().to_string();
    let ks = match &n.keyspace {
        Some(k) => k.name().to_string(),
        None => {
            if schema.keyspace(&name).is_some() {
                return keyspace(db, schema, &name, false, opts);
            }
            current.map(str::to_string).unwrap_or_else(|| name.clone())
        }
    };
    let k = find_keyspace(schema, &ks)?;
    if k.tables.get(&name).is_some_and(|t| !t.is_view())
        || (db.dialect().is_scylla() && paxos::base(db, k, &name).is_some())
    {
        return table(db, schema, &ks, &name, opts);
    }
    if indexed_table(k, &name).is_some() {
        return index(db, schema, &ks, &name, opts);
    }
    // Views, ScyllaDB's index views included (described as informational only).
    if k.tables.get(&name).is_some_and(|t| t.is_view()) {
        return view(db, schema, &ks, &name, opts);
    }
    // ScyllaDB goes on with the user-defined objects: types, functions, aggregates.
    if db.dialect().is_scylla() {
        if k.types.contains_key(&name) {
            return user_type(db, schema, &ks, &name);
        }
        if let Ok(f) = function(db, schema, &ks, &name) {
            return Ok(f);
        }
        if let Ok(a) = aggregate(db, schema, &ks, &name) {
            return Ok(a);
        }
    }
    Err(m::not_found_in_keyspace(&name, &ks))
}

/// `DESCRIBE KEYSPACES`: every keyspace (virtual ones included).
pub fn keyspaces(db: &Database, schema: &Schema) -> Vec<Element> {
    let d = db.dialect();
    sorted_keyspaces(d, schema)
        .into_iter()
        .map(|k| Element {
            keyspace: Some(k.name.clone()),
            kind: Kind::Keyspace,
            name: k.name.clone(),
            signature: None,
            statement: None,
        })
        .collect()
}

/// `DESCRIBE [FULL] SCHEMA`: every (non-system unless `full`) keyspace with its contents, then
/// ScyllaDB's auth objects with `WITH INTERNALS`.
pub fn schema(db: &Database, schema: &Schema, full: bool, opts: Options) -> Vec<Element> {
    let d = db.dialect();
    let mut out = Vec::new();
    for k in sorted_keyspaces(d, schema) {
        if !full && is_system_keyspace(d, &k.name) {
            continue;
        }
        out.extend(keyspace_elements(db, k, false, opts));
    }
    if d.is_scylla() && opts.internals {
        out.extend(scylla::auth_elements(db, opts));
    }
    out
}

/// `DESCRIBE [ONLY] KEYSPACE ks`.
pub fn keyspace(
    db: &Database,
    schema: &Schema,
    name: &str,
    only: bool,
    opts: Options,
) -> Result<Vec<Element>> {
    let k = find_keyspace(schema, name)?;
    Ok(keyspace_elements(db, k, only, opts))
}

/// `DESCRIBE TABLE ks.t`: the table, its indexes and views (and ScyllaDB's CDC log).
pub fn table(
    db: &Database,
    schema: &Schema,
    ks: &str,
    name: &str,
    opts: Options,
) -> Result<Vec<Element>> {
    let d = db.dialect();
    let k = find_keyspace(schema, ks)?;
    let t = match k.tables.get(name) {
        Some(t) if t.is_view() && d.is_scylla() => return Err(m::table_is_view()),
        Some(t) if !t.is_view() => t,
        _ => {
            if d.is_scylla()
                && let Some(base) = paxos::base(db, k, name)
            {
                return Ok(vec![Element::new(
                    ks,
                    Kind::Table,
                    name,
                    paxos::describe(k, base, opts),
                )]);
            }
            return Err(m::table_not_found(name, ks));
        }
    };
    let mut out = table_elements(db, k, t, opts);
    // A CDC log described by name: its CREATE TABLE inside a comment (it is created with CDC).
    if d.is_scylla() && scylla::is_cdc_log(k, t) {
        for e in out.iter_mut() {
            if let Some(stmt) = e.statement.take() {
                e.statement = Some(format!("{}{stmt}\n*/", scylla::CDC_LOG_NOTE));
            }
        }
    }
    if d.is_scylla()
        && let Some(log) = k
            .tables
            .get(&scylla::cdc_log_name(name))
            .filter(|l| scylla::is_cdc_log(k, l))
    {
        out.push(Element::new(
            ks,
            Kind::Table,
            &log.name,
            scylla::cdc_log(log, opts),
        ));
    }
    Ok(out)
}

/// `DESCRIBE INDEX ks.i`.
pub fn index(
    db: &Database,
    schema: &Schema,
    ks: &str,
    name: &str,
    opts: Options,
) -> Result<Vec<Element>> {
    let k = find_keyspace(schema, ks)?;
    let Some((t, _)) = indexed_table(k, name) else {
        return Err(m::index_table_not_found(name, ks));
    };
    Ok(index_elements(db, k, t, opts)
        .into_iter()
        .filter(|e| e.name == name)
        .collect())
}

/// `DESCRIBE MATERIALIZED VIEW ks.v`.
pub fn view(
    db: &Database,
    schema: &Schema,
    ks: &str,
    name: &str,
    opts: Options,
) -> Result<Vec<Element>> {
    let d = db.dialect();
    let k = find_keyspace(schema, ks)?;
    let v = match k.tables.get(name) {
        Some(v) if v.is_view() => v,
        _ => return Err(m::view_not_found(d, name, ks)),
    };
    let stmt = match d {
        Dialect::Scylla => scylla::view(k, v, opts),
        Dialect::Cassandra => cassandra::view(k, v, opts),
    };
    Ok(vec![Element::new(ks, Kind::View, name, stmt)])
}

/// `DESCRIBE TYPE ks.t`.
pub fn user_type(db: &Database, schema: &Schema, ks: &str, name: &str) -> Result<Vec<Element>> {
    let d = db.dialect();
    let k = find_keyspace(schema, ks)?;
    let Some(t) = k.types.get(name) else {
        return Err(m::type_not_found(d, name, ks));
    };
    Ok(vec![Element::new(
        ks,
        Kind::Type,
        name,
        cassandra::user_type(d, t),
    )])
}

/// `DESCRIBE FUNCTION ks.f`: every overload.
pub fn function(db: &Database, schema: &Schema, ks: &str, name: &str) -> Result<Vec<Element>> {
    let d = db.dialect();
    let k = find_keyspace(schema, ks)?;
    let out: Vec<Element> = function_elements(d, k)
        .into_iter()
        .filter(|e| e.name == name)
        .collect();
    if out.is_empty() {
        return Err(m::function_not_found(d, name, ks));
    }
    Ok(out)
}

/// `DESCRIBE AGGREGATE ks.a`: every overload.
pub fn aggregate(db: &Database, schema: &Schema, ks: &str, name: &str) -> Result<Vec<Element>> {
    let d = db.dialect();
    let k = find_keyspace(schema, ks)?;
    let out: Vec<Element> = aggregate_elements(d, k)
        .into_iter()
        .filter(|e| e.name == name)
        .collect();
    if out.is_empty() {
        return Err(m::aggregate_not_found(d, name, ks));
    }
    Ok(out)
}

/// `DESCRIBE TABLES | TYPES | FUNCTIONS | AGGREGATES`: names only, of the current keyspace or
/// of every keyspace.
pub fn listing(
    db: &Database,
    schema: &Schema,
    of: ListingOf,
    current: Option<&str>,
) -> Result<Vec<Element>> {
    let d = db.dialect();
    let kss: Vec<&Arc<Keyspace>> = match current {
        Some(ks) => vec![find_keyspace(schema, ks)?],
        None => sorted_keyspaces(d, schema),
    };
    let mut out = Vec::new();
    for k in kss {
        let mut items: Vec<Element> = match of {
            ListingOf::Tables => user_tables(d, k)
                .into_iter()
                .filter(|t| !(d.is_scylla() && scylla::is_cdc_log(k, t)))
                .map(|t| Element::new(&k.name, Kind::Table, &t.name, String::new()))
                .collect(),
            ListingOf::Types => types_in_order(d, k)
                .into_iter()
                .map(|t| Element::new(&k.name, Kind::Type, &t.name, String::new()))
                .collect(),
            ListingOf::Functions => function_elements(d, k),
            ListingOf::Aggregates => aggregate_elements(d, k),
        };
        if of == ListingOf::Types && d == Dialect::Cassandra {
            items.sort_by(|a, b| name_cmp(d, &a.name, &b.name));
        }
        for mut e in items {
            e.statement = None;
            out.push(e);
        }
    }
    Ok(out)
}

/// `DESCRIBE CLUSTER`: cluster name, partitioner, snitch, and the token ranges of the current
/// (non-system) keyspace.
fn cluster(
    db: &Database,
    schema: &Schema,
    current: Option<&str>,
) -> (Vec<ColumnSpec>, Vec<Vec<Option<Bytes>>>) {
    let d = db.dialect();
    let cfg = db.config();
    let spec = |n: &str, ty: CqlType| ColumnSpec::new("system", "describe", n, ty);
    let mut cols = vec![
        spec("cluster", CqlType::Text),
        spec("partitioner", CqlType::Text),
        spec("snitch", CqlType::Text),
    ];
    let (name, partitioner, snitch) = match d {
        // ScyllaDB's statement reports the default name whatever `system.local` says.
        Dialect::Scylla => (
            "Test Cluster".to_string(),
            "org.apache.cassandra.dht.Murmur3Partitioner",
            "org.apache.cassandra.locator.SimpleSnitch",
        ),
        Dialect::Cassandra => (
            cfg.cluster_name.clone(),
            "Murmur3Partitioner",
            "DynamicEndpointSnitch",
        ),
    };
    let mut row = vec![
        Some(Bytes::from(name)),
        Some(Bytes::from(partitioner)),
        Some(Bytes::from(snitch)),
    ];
    let ranges = current
        .filter(|ks| !is_system_keyspace(d, ks))
        .and_then(|ks| {
            let k = schema.keyspace(ks)?;
            // Tablets keyspaces have no token ring.
            (k.tablets.is_none()).then_some(k)
        });
    if ranges.is_some() {
        cols.push(spec(
            "range_ownership",
            CqlType::map(CqlType::Text, CqlType::list(CqlType::Text, false), false),
        ));
        let endpoint = match d {
            Dialect::Scylla => cfg.address.to_string(),
            Dialect::Cassandra => format!("/{}:7000", cfg.address),
        };
        let mut tokens = crate::system::local::tokens(&cfg.host_id(), cfg.num_tokens);
        // Ordered by range end (`Range` compares right bounds as tokens; the map keys print in
        // insertion order).
        tokens.sort_unstable();
        let one = nosql_types::pack_collection([endpoint.as_bytes()]);
        let keys: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let map = nosql_types::pack_map(keys.iter().map(|k| (k.as_bytes(), &one[..])));
        row.push(Some(map));
    }
    (cols, vec![row])
}

/// Cassandra's DESCRIBE paging: an offset into the rows, valid for one schema version.
fn page(
    d: Dialect,
    schema: &Schema,
    columns: Vec<ColumnSpec>,
    rows: Vec<Vec<Option<Bytes>>>,
    params: &QueryParams,
) -> Result<Response> {
    let size = params
        .page_size
        .filter(|s| *s > 0 && d == Dialect::Cassandra);
    let offset = match (&params.paging_state, size) {
        (Some(state), Some(_)) => decode_paging(state, &schema.version)?,
        _ => 0,
    };
    let mut rows: Vec<_> = rows.into_iter().skip(offset as usize).collect();
    let mut paging_state = None;
    if let Some(size) = size
        && rows.len() >= size as usize
    {
        rows.truncate(size as usize);
        paging_state = Some(encode_paging(&schema.version, offset + size as u64));
    }
    Ok(Response::new(ResponseBody::Rows(Rows {
        metadata: ResultMetadata { columns },
        rows,
        paging_state,
    })))
}

const PAGING_VERSION: u16 = 0x0001;

/// `PagingState(partitionKey = [version, release, schema version, offset], remaining = MAX)` in
/// the v4 serialisation.
fn encode_paging(version: &[u8; 16], offset: u64) -> Bytes {
    let release = crate::system::local::versions(Dialect::Cassandra).0;
    let mut key = Vec::new();
    key.extend_from_slice(&PAGING_VERSION.to_be_bytes());
    key.extend_from_slice(&(release.len() as u16).to_be_bytes());
    key.extend_from_slice(release.as_bytes());
    key.extend_from_slice(version);
    key.extend_from_slice(&offset.to_be_bytes());
    let mut out = Vec::with_capacity(key.len() + 12);
    out.push(key.len() as u8);
    out.extend_from_slice(&key);
    // No row mark; remaining and remaining-in-partition: Integer.MAX_VALUE as vints.
    out.push(0);
    for _ in 0..2 {
        out.extend_from_slice(&[0xf0, 0x7f, 0xff, 0xff, 0xff]);
    }
    Bytes::from(out)
}

fn decode_paging(b: &[u8], version: &[u8; 16]) -> Result<u64> {
    let invalid = || Error::protocol("Invalid value for the paging state");
    let len = *b.first().ok_or_else(invalid)? as usize;
    let key = b.get(1..1 + len).ok_or_else(invalid)?;
    if key.get(..2) != Some(&PAGING_VERSION.to_be_bytes()[..]) {
        return Err(Error::invalid("Incompatible paging state"));
    }
    let rl = u16::from_be_bytes(
        key.get(2..4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    ) as usize;
    let release = key.get(4..4 + rl).ok_or_else(invalid)?;
    let ours = crate::system::local::versions(Dialect::Cassandra).0;
    if release != ours.as_bytes() {
        return Err(m::paging_release_mismatch(
            &String::from_utf8_lossy(release),
            ours,
        ));
    }
    let rest = key.get(4 + rl..).ok_or_else(invalid)?;
    if rest.len() != 24 {
        return Err(invalid());
    }
    if &rest[..16] != version {
        return Err(m::schema_changed_while_paging());
    }
    Ok(u64::from_be_bytes(
        rest[16..24].try_into().map_err(|_| invalid())?,
    ))
}

// ---------------------------------------------------------------------------------------------
// Element collection
// ---------------------------------------------------------------------------------------------

/// Name order: Cassandra's `NAME_COMPARATOR` ignores case, ScyllaDB compares bytes.
fn name_cmp(d: Dialect, a: &str, b: &str) -> std::cmp::Ordering {
    match d {
        Dialect::Scylla => a.as_bytes().cmp(b.as_bytes()),
        Dialect::Cassandra => a
            .to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b)),
    }
}

fn sorted_keyspaces(d: Dialect, schema: &Schema) -> Vec<&Arc<Keyspace>> {
    let mut v: Vec<&Arc<Keyspace>> = schema.keyspaces.values().collect();
    v.sort_by(|a, b| name_cmp(d, &a.name, &b.name));
    v
}

fn find_keyspace<'s>(schema: &'s Schema, name: &str) -> Result<&'s Arc<Keyspace>> {
    schema
        .keyspace(name)
        .ok_or_else(|| m::keyspace_not_found(name))
}

/// `SchemaConstants.isSystemKeyspace` / ScyllaDB's `is_system_keyspace` (not `audit`).
pub fn is_system_keyspace(d: Dialect, name: &str) -> bool {
    let common = [
        "system",
        "system_schema",
        "system_auth",
        "system_distributed",
        "system_traces",
    ];
    common.contains(&name)
        || match d {
            Dialect::Scylla => name == "system_replicated_keys",
            Dialect::Cassandra => crate::system::catalog::is_virtual_keyspace(d, name),
        }
}

/// ScyllaDB's index views (`<index>_index`) and other tables no `DESCRIBE` lists.
fn is_hidden(d: Dialect, k: &Keyspace, name: &str) -> bool {
    d.is_scylla() && scylla::index_of_view(k, name).is_some()
}

/// Tables (not views) of a keyspace in name order.
fn user_tables(d: Dialect, k: &Keyspace) -> Vec<&Arc<Table>> {
    let mut v: Vec<&Arc<Table>> = k
        .tables
        .values()
        .filter(|t| !t.is_view() && !is_hidden(d, k, &t.name))
        .collect();
    v.sort_by(|a, b| name_cmp(d, &a.name, &b.name));
    v
}

/// The table owning index `name`.
fn indexed_table<'k>(
    k: &'k Keyspace,
    name: &str,
) -> Option<(&'k Arc<Table>, &'k crate::schema::IndexMeta)> {
    k.tables
        .values()
        .find_map(|t| t.indexes.iter().find(|i| i.name == name).map(|i| (t, i)))
}

/// User types in dependency order: Cassandra's `Types.sortedStream` (a depth-first walk over
/// the types by name, dependencies first); the built-in keyspaces' in catalog order.
fn types_in_order(d: Dialect, k: &Keyspace) -> Vec<Arc<nosql_types::UdtDef>> {
    if k.is_builtin() {
        return crate::system::catalog::type_entries(d)
            .iter()
            .filter(|(ks, _, _)| *ks == k.name)
            .filter_map(|(_, n, _)| k.types.get(*n).cloned())
            .collect();
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    fn visit(
        k: &Keyspace,
        name: &str,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<Arc<nosql_types::UdtDef>>,
    ) {
        if seen.contains(name) {
            return;
        }
        let Some(t) = k.types.get(name) else {
            return;
        };
        seen.insert(name.to_string());
        for (_, ty) in &t.fields {
            let mut deps = Vec::new();
            udt_refs(ty, &k.name, &mut deps);
            for dep in deps {
                visit(k, &dep, seen, out);
            }
        }
        out.push(t.clone());
    }
    for name in k.types.keys() {
        visit(k, name, &mut seen, &mut out);
    }
    out
}

fn udt_refs(ty: &CqlType, ks: &str, out: &mut Vec<String>) {
    match ty {
        CqlType::Udt(def, _) => {
            if def.keyspace == ks {
                out.push(def.name.clone());
            }
        }
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } | CqlType::Vector { elem, .. } => {
            udt_refs(elem, ks, out)
        }
        CqlType::Map { key, value, .. } => {
            udt_refs(key, ks, out);
            udt_refs(value, ks, out);
        }
        CqlType::Tuple(ts) => ts.iter().for_each(|t| udt_refs(t, ks, out)),
        _ => {}
    }
}

fn function_elements(d: Dialect, k: &Keyspace) -> Vec<Element> {
    let mut v: Vec<Element> = k
        .functions
        .iter()
        .map(|f| {
            let (stmt, sig) = match d {
                Dialect::Scylla => (scylla::function(k, f), None),
                Dialect::Cassandra => (
                    cassandra::function(k, f),
                    Some(format!(
                        "({})",
                        f.argument_types
                            .iter()
                            .map(|t| cassandra::function_type(t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                ),
            };
            Element {
                keyspace: Some(k.name.clone()),
                kind: Kind::Function,
                name: f.name.clone(),
                signature: sig,
                statement: Some(stmt),
            }
        })
        .collect();
    v.sort_by(|a, b| {
        name_cmp(
            d,
            &format!("{}{}", a.name, a.signature.as_deref().unwrap_or("")),
            &format!("{}{}", b.name, b.signature.as_deref().unwrap_or("")),
        )
    });
    v
}

fn aggregate_elements(d: Dialect, k: &Keyspace) -> Vec<Element> {
    let mut v: Vec<Element> = k
        .aggregates
        .iter()
        .map(|a| {
            let (stmt, sig) = match d {
                Dialect::Scylla => (scylla::aggregate(k, a), None),
                Dialect::Cassandra => (
                    cassandra::aggregate(k, a),
                    Some(format!("({})", a.argument_types.join(", "))),
                ),
            };
            Element {
                keyspace: Some(k.name.clone()),
                kind: Kind::Aggregate,
                name: a.name.clone(),
                signature: sig,
                statement: Some(stmt),
            }
        })
        .collect();
    v.sort_by(|a, b| {
        name_cmp(
            d,
            &format!("{}{}", a.name, a.signature.as_deref().unwrap_or("")),
            &format!("{}{}", b.name, b.signature.as_deref().unwrap_or("")),
        )
    });
    v
}

/// `getKeyspaceElements`.
fn keyspace_elements(db: &Database, k: &Keyspace, only: bool, opts: Options) -> Vec<Element> {
    let d = db.dialect();
    let stmt = match d {
        Dialect::Scylla => scylla::keyspace(k),
        Dialect::Cassandra => cassandra::keyspace(k),
    };
    let mut out = vec![Element::new(&k.name, Kind::Keyspace, &k.name, stmt)];
    if only {
        return out;
    }
    for t in types_in_order(d, k) {
        out.push(Element::new(
            &k.name,
            Kind::Type,
            &t.name,
            cassandra::user_type(d, &t),
        ));
    }
    out.extend(function_elements(d, k));
    out.extend(aggregate_elements(d, k));
    for t in user_tables(d, k) {
        if d.is_scylla() && scylla::is_cdc_log(k, t) {
            out.push(Element::new(
                &k.name,
                Kind::Table,
                &t.name,
                scylla::cdc_log(t, opts),
            ));
            continue;
        }
        out.extend(table_elements(db, k, t, opts));
    }
    out
}

/// A table followed by its indexes and views (`getTableElements`).
fn table_elements(db: &Database, k: &Keyspace, t: &Arc<Table>, opts: Options) -> Vec<Element> {
    let d = db.dialect();
    let stmt = match d {
        Dialect::Scylla => scylla::table(k, t, opts),
        Dialect::Cassandra => cassandra::table(k, t, opts),
    };
    let mut out = vec![Element::new(&k.name, Kind::Table, &t.name, stmt)];
    out.extend(index_elements(db, k, t, opts));
    let mut views: Vec<&Arc<Table>> = k
        .tables
        .values()
        .filter(|v| {
            v.view.as_ref().is_some_and(|m| m.base_table_id == t.id) && !is_hidden(d, k, &v.name)
        })
        .collect();
    views.sort_by(|a, b| name_cmp(d, &a.name, &b.name));
    for v in views {
        let stmt = match d {
            Dialect::Scylla => scylla::view(k, v, opts),
            Dialect::Cassandra => cassandra::view(k, v, opts),
        };
        out.push(Element::new(&k.name, Kind::View, &v.name, stmt));
    }
    out
}

fn index_elements(db: &Database, k: &Keyspace, t: &Arc<Table>, opts: Options) -> Vec<Element> {
    let d = db.dialect();
    let mut out: Vec<Element> = t
        .indexes
        .iter()
        .map(|i| {
            let stmt = match d {
                Dialect::Scylla => scylla::index(k, t, i, opts),
                Dialect::Cassandra => cassandra::index(t, i),
            };
            Element::new(&k.name, Kind::Index, &i.name, stmt)
        })
        .collect();
    if k.is_builtin() {
        for (ks, table, name, stmt) in crate::system::catalog::indexes(d) {
            if *ks == k.name && *table == t.name {
                out.push(Element::new(ks, Kind::Index, name, stmt.to_string()));
            }
        }
    }
    out.sort_by(|a, b| name_cmp(d, &a.name, &b.name));
    out
}

/// Whether a keyspace is virtual (Cassandra's `system_views`, `system_virtual_schema`).
fn is_virtual(k: &Keyspace) -> bool {
    k.kind == KeyspaceKind::Virtual
}

/// Map options as `{'k': 'v'}` (sorted).
fn options_map(m: &BTreeMap<String, String>, d: Dialect) -> String {
    let mut b = builder::CqlBuilder::new(d);
    b.map(m, true);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_state_round_trips() {
        let v = [7u8; 16];
        let p = encode_paging(&v, 42);
        assert_eq!(p[0] as usize, 33);
        assert_eq!(&p[3..5], &[0, 5]);
        assert_eq!(&p[5..10], b"5.0.9");
        assert_eq!(decode_paging(&p, &v).unwrap(), 42);
        let e = decode_paging(&p, &[8u8; 16]).unwrap_err();
        assert_eq!(e.message, m::schema_changed_while_paging().message);
        assert!(decode_paging(&p[..10], &v).is_err());
        assert!(decode_paging(&[], &v).is_err());
    }

    #[test]
    fn name_order_per_dialect() {
        use std::cmp::Ordering::*;
        assert_eq!(name_cmp(Dialect::Scylla, "IndexInfo", "batchlog"), Less);
        assert_eq!(
            name_cmp(Dialect::Cassandra, "IndexInfo", "batchlog"),
            Greater
        );
        assert_eq!(
            name_cmp(Dialect::Cassandra, "compaction_history", "IndexInfo"),
            Less
        );
    }
}
