//! `CREATE INDEX` and `DROP INDEX`.
//!
//! Cassandra's rules are ported from `CreateIndexStatement`, `IndexAttributes`,
//! `IndexMetadata.validate` and `StorageAttachedIndex.validateOptions` (Apache-2.0), in their
//! order; ScyllaDB's (local indexes, `vector_index`, the stored target forms, its messages) were
//! learned from the reference image. The index lives in the base table's `indexes`
//! (`system_schema.indexes`); creating one bumps the base table's version (prepared statements on
//! it are re-prepared), creates the index table (ScyllaDB: the `<name>_index` view; Cassandra: a
//! hidden table) and backfills it before answering.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use nosql_cql::ast::{self, IndexTarget as AstTarget, IndexTargetKind, Statement, StatementKind};
use nosql_types::{CqlType, Dialect};

use super::{IndexClass, IndexDef, TargetKind};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{Access, StatementHandler};
use crate::messages as msg;
use crate::response::{ChangeTarget, ChangeType, QueryParams, Response, SchemaChange};
use crate::schema::ddl::{Change, commit};
use crate::schema::params::{self, Properties};
use crate::schema::{ColumnKind, IndexMeta, Keyspace, Table};
use crate::session::SessionState;
use crate::views::msg as vmsg;

/// Executes `CREATE INDEX` / `DROP INDEX`.
pub struct Handler;

impl StatementHandler for Handler {
    fn handles(&self, stmt: &StatementKind<'_>) -> bool {
        matches!(
            stmt,
            StatementKind::CreateIndex(_) | StatementKind::DropIndex(_)
        )
    }

    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a mut SessionState,
        stmt: &'a Statement<'static>,
        params: &'a QueryParams,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async move {
            let current = params.keyspace.clone().or_else(|| session.keyspace.clone());
            match &stmt.kind {
                StatementKind::CreateIndex(c) => create(db, session, c, current.as_deref()).await,
                StatementKind::DropIndex(x) => drop_index(db, session, x, current.as_deref()).await,
                other => Err(crate::hooks::unsupported(other)),
            }
        })
    }
}

/// The index implementation a `CREATE INDEX` asks for.
#[derive(Clone, Debug, PartialEq)]
enum Using {
    /// The default secondary index.
    Default,
    Sai(String),
    Sasi,
    /// ScyllaDB `vector_index` (the class as written: it is stored so).
    VectorIndex(String),
    /// ScyllaDB `fulltext_index` (as written).
    Fulltext(String),
    Unknown(String),
}

fn using(d: Dialect, custom: bool, class: Option<&str>) -> Result<(bool, Using)> {
    match (d, custom, class) {
        (_, false, None) => Ok((false, Using::Default)),
        // Cassandra's default implementation (`default_secondary_index: legacy_local_table`) is
        // looked up as a custom class when CUSTOM is written without USING.
        (Dialect::Cassandra, true, None) => Ok((true, Using::Unknown("legacy_local_table".into()))),
        (Dialect::Scylla, true, None) => Err(Error::invalid(
            "CUSTOM index requires specifying the index class",
        )),
        (Dialect::Cassandra, false, Some(c)) if c.eq_ignore_ascii_case("legacy_local_table") => {
            Ok((false, Using::Default))
        }
        (_, _, Some(c)) => {
            let u = if super::is_sai_class(c) {
                Using::Sai(c.to_string())
            } else if c == super::SASI_CLASS && !d.is_scylla() {
                Using::Sasi
            } else if c.eq_ignore_ascii_case("vector_index") && d.is_scylla() {
                Using::VectorIndex(c.to_string())
            } else if c.eq_ignore_ascii_case("fulltext_index") && d.is_scylla() {
                Using::Fulltext(c.to_string())
            } else {
                Using::Unknown(c.to_string())
            };
            Ok((true, u))
        }
    }
}

/// `IndexMetadata.generateDefaultIndexName` + `findAvailableIndexName`. ScyllaDB also passes
/// over a name whose index view name (`<name>_index`) a table already has.
fn default_name(ks: &Keyspace, table: &str, column: &str, d: Dialect) -> String {
    default_name_from(ks, &format!("{table}_{column}_idx"), d)
}

/// `raw` (its identifier characters), or the first `raw_N` not taken.
fn default_name_from(ks: &Keyspace, raw: &str, d: Dialect) -> String {
    let base: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let taken = |n: &str| {
        keyspace_has_index(ks, n)
            || (d.is_scylla() && ks.tables.contains_key(&format!("{n}_index")))
    };
    if !taken(&base) {
        return base;
    }
    (1..)
        .map(|i| format!("{base}_{i}"))
        .find(|n| !taken(n))
        .unwrap_or(base)
}

pub fn keyspace_has_index(ks: &Keyspace, name: &str) -> bool {
    ks.tables
        .values()
        .any(|t| t.indexes.iter().any(|i| i.name == name))
}

/// The stored target (`system_schema.indexes.options.target`).
fn target_text(
    d: Dialect,
    col: &crate::schema::Column,
    kind: TargetKind,
    local: Option<&Table>,
) -> String {
    let name = nosql_types::quote_identifier(&col.name, d).into_owned();
    if let Some(base) = local {
        let pk: Vec<String> = base
            .partition_key_columns()
            .map(|c| format!("\"{}\"", c.name.replace('"', "\\\"")))
            .collect();
        return format!(
            "{{\"pk\":[{}],\"ck\":[\"{}\"]}}",
            pk.join(","),
            col.name.replace('"', "\\\"")
        );
    }
    match (d, kind, &col.ty) {
        (_, TargetKind::Simple, _) => name,
        // ScyllaDB stores a frozen collection's full index as the column, a set's values index
        // as its keys.
        (Dialect::Scylla, TargetKind::Full, _) => name,
        (Dialect::Scylla, TargetKind::Values, CqlType::Set { .. }) => format!("keys({name})"),
        (_, k, _) => format!("{}({name})", k.name()),
    }
}

/// `CreateIndexStatement.validateIndexTarget` (Cassandra) and ScyllaDB's equivalents.
fn validate_target(
    d: Dialect,
    table: &Table,
    ci: usize,
    kind: TargetKind,
    explicit: bool,
) -> Result<()> {
    let col = &table.columns[ci];
    let ty = &col.ty;
    if ty.references_duration() {
        return Err(Error::invalid(if ty.is_collection() {
            "Secondary indexes are not supported on collections containing durations"
        } else if ty.is_tuple() {
            "Secondary indexes are not supported on tuples containing durations"
        } else if ty.is_udt() {
            "Secondary indexes are not supported on UDTs containing durations"
        } else {
            "Secondary indexes are not supported on duration columns"
        }));
    }
    if col.kind == ColumnKind::PartitionKey && (table.partition_key.len() == 1 || d.is_scylla()) {
        if d.is_scylla() {
            if table.partition_key.len() == 1 {
                return Err(Error::invalid(format!(
                    "Cannot create secondary index on partition key column {}",
                    col.name
                )));
            }
        } else {
            return Err(Error::invalid(format!(
                "Cannot create secondary index on the only partition key column {}",
                col.name
            )));
        }
    }
    let frozen_collection = ty.is_frozen_collection();
    let is_map = matches!(ty, CqlType::Map { .. }) && ty.is_multicell();
    match d {
        Dialect::Cassandra => {
            if frozen_collection && kind != TargetKind::Full {
                return Err(Error::invalid(format!(
                    "Cannot create {}() index on frozen column {}. Frozen collections are immutable and must be fully indexed by using the 'full({})' modifier",
                    kind_word(kind),
                    col.name,
                    nosql_types::quote_identifier(&col.name, d)
                )));
            }
            if !frozen_collection && kind == TargetKind::Full {
                return Err(Error::invalid(
                    "full() indexes can only be created on frozen collections",
                ));
            }
            if !ty.is_collection() && kind != TargetKind::Simple {
                return Err(Error::invalid(format!(
                    "Cannot create {}() index on {}. Non-collection columns only support simple indexes",
                    kind_word(kind),
                    col.name
                )));
            }
            if !is_map && matches!(kind, TargetKind::Keys | TargetKind::Entries) {
                return Err(Error::invalid(format!(
                    "Cannot create index on {} of column {} with non-map type",
                    kind_word(kind),
                    col.name
                )));
            }
            if ty.is_udt() && ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Cannot create index on non-frozen UDT column {}",
                    col.name
                )));
            }
        }
        Dialect::Scylla => {
            if frozen_collection && kind != TargetKind::Full {
                return Err(Error::invalid(if explicit {
                    format!(
                        "Cannot create index on {}() of frozen collection column {}",
                        kind_word(kind),
                        col.name
                    )
                } else {
                    format!(
                        "Cannot create index on value of frozen collection column {}",
                        col.name
                    )
                }));
            }
            if kind == TargetKind::Full && !frozen_collection {
                return Err(Error::invalid(
                    "full() indexes can only be created on frozen collections",
                ));
            }
            if matches!(kind, TargetKind::Keys | TargetKind::Entries)
                && ty.is_collection()
                && !is_map
            {
                return Err(Error::invalid(format!(
                    "Cannot create secondary index on {} of column {} with non-map type",
                    kind_word(kind),
                    col.name
                )));
            }
            if !ty.is_collection()
                && matches!(
                    kind,
                    TargetKind::Values | TargetKind::Keys | TargetKind::Entries
                )
            {
                return Err(Error::invalid(format!(
                    "Cannot create index on {}() of column {}; only non-frozen collections support {}() indexes",
                    kind_word(kind),
                    col.name,
                    kind_word(kind)
                )));
            }
            if ty.is_udt() && ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Cannot create secondary index on UDT column {}",
                    col.name
                )));
            }
        }
    }
    Ok(())
}

fn kind_word(k: TargetKind) -> &'static str {
    match k {
        TargetKind::Simple => "simple",
        TargetKind::Values => "values",
        TargetKind::Keys => "keys",
        TargetKind::Entries => "entries",
        TargetKind::Full => "full",
    }
}

fn ast_kind(k: IndexTargetKind) -> TargetKind {
    match k {
        IndexTargetKind::Simple => TargetKind::Simple,
        IndexTargetKind::Values => TargetKind::Values,
        IndexTargetKind::Keys => TargetKind::Keys,
        IndexTargetKind::Entries => TargetKind::Entries,
        IndexTargetKind::Full => TargetKind::Full,
    }
}

/// `WITH OPTIONS = {...}` of `CREATE INDEX` (ScyllaDB's other properties are its view's, see
/// [`view_properties`]).
fn options(c: &ast::CreateIndex, d: Dialect) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for p in &c.properties {
        if p.name.name() != "options" {
            if d.is_scylla() {
                continue;
            }
            return Err(Error::syntax(format!(
                "Unknown property '{}'",
                p.name.name()
            )));
        }
        match &p.value {
            ast::PropertyValue::Map(m) => {
                for (k, v) in m {
                    out.insert(k.text.to_string(), v.text.to_string());
                }
            }
            ast::PropertyValue::Simple(_) => {
                return Err(Error::syntax(
                    "Invalid value for property 'options'. It should be a map.",
                ));
            }
        }
    }
    Ok(out)
}

/// ScyllaDB's view properties of an index (any `CREATE MATERIALIZED VIEW` property but
/// `CLUSTERING ORDER BY`), applied to its index view. Checked first: none on a vector index,
/// known names, the `id` (well formed, not taken), `COMPACT STORAGE`, the clustering order and
/// `cdc`; their values are checked with the view ([`apply_view_properties`]).
fn view_properties<'p, 'a>(
    schema: &crate::schema::Schema,
    c: &'p ast::CreateIndex<'a>,
    external: Option<&str>,
) -> Result<(Properties<'p, 'a>, Option<[u8; 16]>)> {
    let props: Vec<&ast::Property> = c
        .properties
        .iter()
        .filter(|p| p.name.name() != "options")
        .collect();
    if let Some(cls) = external
        && (!props.is_empty() || !c.view_options.is_empty())
    {
        return Err(Error::invalid(format!(
            "You cannot use view properties with a {cls}"
        )));
    }
    // The read repair chances, accepted (and ignored) on tables and views, are unknown here.
    let valid: Vec<&str> = params::valid_table_options(Dialect::Scylla)
        .iter()
        .copied()
        .filter(|o| !o.ends_with("read_repair_chance"))
        .collect();
    let p = Properties::new(props.iter().copied(), &valid, Dialect::Scylla)?;
    let id = match p.string("id")? {
        Some(s) => {
            let id =
                crate::uuid::parse(s.trim()).ok_or_else(|| Error::config("Invalid table id"))?;
            if let Some(t) = schema.table_by_id(&id) {
                return Err(Error::invalid(format!(
                    "Table with ID {} already exists: {}.{}",
                    crate::uuid::format(&id),
                    t.keyspace,
                    t.name
                )));
            }
            Some(id)
        }
        None => None,
    };
    if c.view_options
        .iter()
        .any(|o| matches!(o, ast::TableOption::CompactStorage))
    {
        return Err(Error::invalid(
            "Cannot use 'COMPACT STORAGE' when defining a materialized view",
        ));
    }
    if !c.view_options.is_empty() {
        return Err(Error::invalid(
            "Indexes do not allow for specifying the clustering order",
        ));
    }
    if p.has("cdc") {
        return Err(Error::invalid(vmsg::VIEW_CDC));
    }
    Ok((p, id))
}

/// The index view with its view properties (validated as a view's).
fn apply_view_properties(view: Table, p: &Properties) -> Result<Table> {
    if p.props.is_empty() {
        return Ok(view);
    }
    let mut vparams = view.params.clone();
    params::apply(&mut vparams, p, true)?;
    if vparams.default_time_to_live != 0 {
        return Err(Error::invalid(
            "Cannot set or alter default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table.",
        ));
    }
    let mut t = view.rebuild(view.columns.clone(), vparams);
    t.version = view.version;
    Ok(t)
}

/// The statement's `OPTIONS` in the order written.
fn raw_options(c: &ast::CreateIndex) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in &c.properties {
        if let ast::PropertyValue::Map(m) = &p.value {
            for (k, v) in m {
                out.push((k.text.to_string(), v.text.to_string()));
            }
        }
    }
    out
}

/// The Cassandra 5.0 SAI types (`StorageAttachedIndex.SUPPORTED_TYPES`).
fn sai_supported(ty: &CqlType) -> bool {
    matches!(
        ty,
        CqlType::Ascii
            | CqlType::Bigint
            | CqlType::Date
            | CqlType::Double
            | CqlType::Float
            | CqlType::Int
            | CqlType::Smallint
            | CqlType::Text
            | CqlType::Time
            | CqlType::Timestamp
            | CqlType::Timeuuid
            | CqlType::Tinyint
            | CqlType::Uuid
            | CqlType::Inet
            | CqlType::Varint
            | CqlType::Decimal
            | CqlType::Boolean
    )
}

const SAI_OPTIONS: &[&str] = &[
    "target",
    "class_name",
    "maximum_node_connections",
    "construction_beam_width",
    "similarity_function",
    "optimize_for",
    "case_sensitive",
    "normalize",
    "ascii",
];

/// `StorageAttachedIndex.validateOptions` (with the new index already in `indexes`).
fn validate_sai(
    table: &Table,
    def: &IndexDef,
    indexes: &[IndexMeta],
    opts: &BTreeMap<String, String>,
    custom_parameters: bool,
    warnings: &mut Vec<String>,
) -> Result<()> {
    // The table's indexes are validated in order: an existing SAI index on the same target fails
    // the duplicate check before the new one's options are looked at.
    let same = indexes
        .iter()
        .filter_map(|m| super::def(table, m))
        .filter(|o| o.class == IndexClass::Sai && o.column == def.column && o.target == def.target)
        .count();
    let col = &table.columns[def.column];
    if same > 1 {
        return Err(Error::invalid(format!(
            "Cannot create more than one storage-attached index on the same column: {}",
            col.name
        )));
    }
    let unknown: Vec<&String> = opts
        .keys()
        .filter(|k| *k != "class_name" && !SAI_OPTIONS.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        let names: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
        return Err(Error::config(format!(
            "Properties specified [{}] are not understood by StorageAttachedIndex",
            names.join(", ")
        )));
    }
    let analysis: Vec<(&String, &String)> = opts
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "case_sensitive" | "normalize" | "ascii"))
        .collect();
    if col.is_primary_key() && !analysis.is_empty() {
        let shown: Vec<String> = analysis
            .iter()
            .map(|(k, v)| format!("'{k}': '{v}'"))
            .collect();
        return Err(Error::invalid(format!(
            "Analysis options are not supported on primary key columns, but found {{{}}}",
            shown.join(", ")
        )));
    }
    let term = match (&col.ty, def.target) {
        (CqlType::List { elem, .. } | CqlType::Set { elem, .. }, TargetKind::Values) => {
            (**elem).clone()
        }
        (CqlType::Map { key, .. }, TargetKind::Keys) => (**key).clone(),
        (CqlType::Map { value, .. }, TargetKind::Values) => (**value).clone(),
        (t, _) => t.clone(),
    };
    if !analysis.is_empty() && !matches!(term, CqlType::Text | CqlType::Ascii) {
        return Err(Error::invalid(format!(
            "CQL type {} cannot be analyzed.",
            nosql_types::cql_name(&term)
        )));
    }
    let vector_opts = [
        "maximum_node_connections",
        "construction_beam_width",
        "similarity_function",
        "optimize_for",
    ];
    if vector_opts.iter().any(|k| opts.contains_key(*k)) {
        if !term.is_vector() {
            return Err(Error::invalid(format!(
                "CQL type {} cannot have vector options",
                nosql_types::cql_name(&term)
            )));
        }
        // `IndexWriterConfig.fromOptions` (the index name is not known yet: "null").
        for (k, prop, invalid, range, max) in [
            (
                "maximum_node_connections",
                "Maximum node connections",
                "Maximum number of connections",
                "Maximum number of connections for index null",
                512,
            ),
            (
                "construction_beam_width",
                "Construction beam width",
                "Construction beam width",
                "Construction beam width for index null",
                3200,
            ),
        ] {
            let Some(v) = opts.get(k) else {
                continue;
            };
            if !custom_parameters {
                return Err(Error::invalid(format!(
                    "{prop} cannot be set without enabling SAI_VECTOR_ALLOW_CUSTOM_PARAMETERS"
                )));
            }
            let Ok(n) = v.parse::<i32>() else {
                return Err(Error::invalid(format!(
                    "{invalid} {v} is not a valid integer for index null"
                )));
            };
            if n <= 0 || n > max {
                return Err(Error::invalid(format!(
                    "{range} cannot be <= 0 or > {max}, was {n}"
                )));
            }
        }
        if let Some(f) = opts.get("similarity_function") {
            let up = f.to_uppercase();
            if !matches!(up.as_str(), "EUCLIDEAN" | "DOT_PRODUCT" | "COSINE") {
                return Err(Error::invalid(format!(
                    "Similarity function {up} was not recognized for index null. Valid values are: EUCLIDEAN, DOT_PRODUCT, COSINE"
                )));
            }
        }
        if let Some(f) = opts.get("optimize_for") {
            let up = f.to_uppercase();
            if !matches!(up.as_str(), "LATENCY" | "RECALL") {
                return Err(Error::invalid(format!(
                    "optimize_for '{up}' was not recognized for index null. Valid values are: LATENCY, RECALL"
                )));
            }
        }
    }
    let frozen = term.is_frozen_collection()
        || matches!(&term, CqlType::Tuple(_))
        || matches!(&term, CqlType::Udt(_, true));
    if def.target == TargetKind::Entries {
        if let CqlType::Map { key, value, .. } = &col.ty {
            for t in [key, value] {
                if !sai_supported(t) && !t.is_frozen_collection() {
                    return Err(Error::invalid(format!(
                        "Unsupported type: {}",
                        nosql_types::cql_name(t)
                    )));
                }
            }
        }
    } else if !sai_supported(&term) && !frozen {
        if let CqlType::Vector { elem, dim } = &term {
            if **elem != CqlType::Float {
                return Err(Error::invalid(
                    "SAI ANN indexes are only allowed on vector columns with float elements",
                ));
            }
            let cosine = opts
                .get("similarity_function")
                .is_none_or(|f| f.eq_ignore_ascii_case("cosine"));
            if *dim == 1 && cosine {
                return Err(Error::invalid(
                    "Cosine similarity is not supported for single-dimension vectors",
                ));
            }
            warnings.push(vmsg::SAI_VECTOR_WARNING.to_string());
        } else {
            return Err(Error::invalid(format!(
                "Unsupported type: {}",
                nosql_types::cql_name(&term)
            )));
        }
    }
    Ok(())
}

/// ScyllaDB's `fulltext_index` target: exactly one regular or clustering column (static ones
/// too) of a text type.
fn fulltext_target(d: Dialect, table: &Table, c: &ast::CreateIndex) -> Result<usize> {
    let [AstTarget::Column(k, id)] = &c.targets[..] else {
        return Err(Error::invalid(
            "Fulltext index must have exactly one target column",
        ));
    };
    let name = id.name();
    let Some(ci) = table.column_index(&name) else {
        return Err(Error::invalid(format!("Column {name} not found in schema")));
    };
    let kind = ast_kind(*k);
    if kind != TargetKind::Simple {
        validate_target(d, table, ci, kind, true)?;
    }
    let col = &table.columns[ci];
    if !matches!(col.ty, CqlType::Text | CqlType::Ascii) {
        return Err(Error::invalid(format!(
            "Fulltext index is only supported on text, varchar, or ascii columns, but column {name} has an incompatible type"
        )));
    }
    if col.kind == ColumnKind::PartitionKey && table.partition_key.len() == 1 {
        return Err(Error::invalid(format!(
            "Cannot create secondary index on partition key column {name}"
        )));
    }
    Ok(ci)
}

/// ScyllaDB's `fulltext_index`: a tablets keyspace, the base table's CDC options, and the
/// options (`analyzer`, `positions`), checked in the reference's map order.
fn validate_fulltext(table: &Table, tablets: bool, opts: &BTreeMap<String, String>) -> Result<()> {
    if !tablets {
        return Err(Error::invalid(
            "Creating a fulltext index requires the base table's keyspace to use tablets.\n\
             Please alter the keyspace to use tablets and try again.",
        ));
    }
    crate::cdc::check_fulltext_index(table)?;
    super::vector_ddl::validate_fulltext_options(opts)
}

/// ScyllaDB's vector index: its base table's CDC options and the index options (the targets
/// are checked by `vector_ddl::targets`).
fn validate_scylla_vector(table: &Table, opts: &BTreeMap<String, String>) -> Result<()> {
    crate::cdc::check_vector_index(table)?;
    super::vector_ddl::validate_options(opts)
}

async fn create(
    db: &Arc<Database>,
    session: &SessionState,
    c: &ast::CreateIndex<'static>,
    current: Option<&str>,
) -> Result<Response> {
    let d = db.dialect();
    let ks_name = match (&c.table.keyspace, current) {
        (Some(k), _) => k.name().to_string(),
        (None, Some(k)) => k.to_string(),
        (None, None) => return Err(msg::no_keyspace()),
    };
    let tname = c.table.name.name().to_string();
    db.hooks().authorizer.authorize(
        db,
        session,
        &Access::Schema {
            keyspace: &ks_name,
            table: &tname,
            statement: if c.custom {
                "CREATE CUSTOM INDEX"
            } else {
                "CREATE INDEX"
            },
        },
    )?;
    let _g = db.ddl.lock().await;
    let schema = db.schema();
    let opts = options(c, d)?;
    let (custom, class) = using(d, c.custom, c.using.as_deref())?;
    // `IndexAttributes.validate`.
    if !custom && !opts.is_empty() {
        return Err(Error::invalid(
            "Cannot specify options for a non-CUSTOM index",
        ));
    }
    for k in ["class_name", "target"] {
        if opts.contains_key(k) && !d.is_scylla() {
            return Err(Error::invalid(format!(
                "Cannot specify {k} as a CUSTOM option"
            )));
        }
    }
    if let Using::Unknown(cls) = &class
        && d.is_scylla()
    {
        return Err(Error::invalid(format!(
            "Non-supported custom class '{cls}' provided"
        )));
    }
    if d.is_scylla()
        && let Some(k) = ["index_version", "class_name"]
            .into_iter()
            .find(|k| opts.contains_key(*k))
    {
        return Err(Error::invalid(format!(
            "Cannot specify {k} as a CUSTOM option"
        )));
    }
    let (view_props, view_id) = if d.is_scylla() {
        let external = match class {
            Using::VectorIndex(_) | Using::Sai(_) => Some("vector_index"),
            Using::Fulltext(_) => Some("fulltext_index"),
            _ => None,
        };
        let (p, id) = view_properties(&schema, c, external)?;
        (Some(p), id)
    } else {
        (None, None)
    };
    if class == Using::Sasi && !db.settings().sasi_indexes_enabled {
        return Err(Error::invalid(
            "SASI indexes are disabled. Enable in cassandra.yaml to use.",
        ));
    }
    let Some(ks) = schema.keyspace(&ks_name) else {
        return Err(msg::ddl_unknown_keyspace(d, &ks_name));
    };
    if ks.is_builtin() {
        return Err(Error::unauthorized(format!(
            "{ks_name} keyspace is not user-modifiable."
        )));
    }
    let Some(table) = ks.tables.get(&tname).cloned() else {
        return Err(match d {
            Dialect::Scylla => msg::unconfigured_table(d, &tname),
            Dialect::Cassandra => Error::invalid(format!("Table '{tname}' doesn't exist")),
        });
    };
    let index_name = c.name.as_ref().map(|n| n.name().to_string());
    // ScyllaDB's name limit (`schema::NAME_LENGTH`).
    if d.is_scylla()
        && let Some(n) = &index_name
        && n.chars().count() > 192
    {
        return Err(Error::invalid(format!(
            "index names shouldn't be more than 192 characters long (got \"{n}\")"
        )));
    }
    // (Vector and full-text indexes have no view, and no warning.)
    let external = match class {
        Using::VectorIndex(_) | Using::Fulltext(_) => true,
        // (CassIO's SAI `ENTRIES` index may become a regular one.)
        Using::Sai(_) => !c
            .targets
            .iter()
            .any(|t| matches!(t, AstTarget::Column(IndexTargetKind::Entries, _))),
        _ => false,
    };
    let tablets_warning = || -> Vec<String> {
        if d.is_scylla() && ks.tablets.is_some() && !external {
            vec![vmsg::TABLETS_INDEX_WARNING.to_string()]
        } else {
            Vec::new()
        }
    };
    // Cassandra warns about SASI whatever the statement does (`CreateIndexStatement.clientWarnings`).
    let class_warning = || -> Vec<String> {
        if class == Using::Sasi {
            vec![vmsg::SASI_WARNING.to_string()]
        } else {
            Vec::new()
        }
    };
    // ScyllaDB's IF NOT EXISTS goes on to the duplicate check (and fails on the index view's
    // name).
    // A vector index has no view: IF NOT EXISTS on a taken name does nothing.
    let no_view = matches!(
        class,
        Using::VectorIndex(_) | Using::Sai(_) | Using::Fulltext(_)
    );
    if let Some(n) = &index_name
        && keyspace_has_index(ks, n)
        && !(d.is_scylla() && c.if_not_exists && !no_view)
    {
        if c.if_not_exists {
            let mut w = tablets_warning();
            w.extend(class_warning());
            return Ok(Response::void().with_warnings(w));
        }
        return Err(Error::invalid(match d {
            Dialect::Scylla => "Index already exists".to_string(),
            Dialect::Cassandra => format!("Index '{n}' already exists"),
        }));
    }
    if table.is_counter() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => "Secondary indexes are not supported on counter tables",
            Dialect::Cassandra => "Secondary indexes on counter tables aren't supported",
        }));
    }
    if table.is_view() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => "Secondary indexes are not supported on materialized views",
            Dialect::Cassandra => "Secondary indexes on materialized views aren't supported",
        }));
    }
    // Targets. ScyllaDB's vector index has its own (a local partition, filtering columns), and
    // takes SAI class names (but CassIO's `ENTRIES` index when that is rewritten).
    let cassio_rewrite = d.is_scylla()
        && matches!(class, Using::Sai(_))
        && db.settings().scylla.enable_cassio_compatibility
        && c.targets
            .iter()
            .any(|t| matches!(t, AstTarget::Column(IndexTargetKind::Entries, _)));
    let vector_path =
        d.is_scylla() && !cassio_rewrite && matches!(class, Using::VectorIndex(_) | Using::Sai(_));
    let mut vector_target = None;
    let (local, targets) = if matches!(class, Using::Fulltext(_)) {
        let ci = fulltext_target(d, &table, c)?;
        (false, vec![(ci, TargetKind::Simple, false)])
    } else if vector_path {
        let (vt, ci) = super::vector_ddl::targets(&table, c, matches!(class, Using::Sai(_)))?;
        vector_target = Some(vt);
        (false, vec![(ci, TargetKind::Simple, false)])
    } else {
        let mut local: bool = false;
        let mut targets: Vec<(usize, TargetKind, bool)> = Vec::new();
        for t in &c.targets {
            match t {
                AstTarget::Column(k, id) => {
                    let name = id.name();
                    let Some(ci) = table.column_index(&name) else {
                        return Err(match d {
                            Dialect::Scylla => Error::invalid(format!(
                                "No column definition found for column {name}"
                            )),
                            Dialect::Cassandra => msg::undefined_column(d, &ks_name, &tname, &name),
                        });
                    };
                    targets.push((ci, ast_kind(*k), *k != IndexTargetKind::Simple));
                }
                AstTarget::PartitionKey(cols) => {
                    local = true;
                    let mut names = Vec::new();
                    for id in cols {
                        let name = id.name();
                        let Some(ci) = table.column_index(&name) else {
                            return Err(Error::invalid(format!(
                                "No column definition found for column {name}"
                            )));
                        };
                        if table.columns[ci].kind != ColumnKind::PartitionKey {
                            return Err(Error::invalid(format!(
                                "Local index definition must contain full partition key only. Redundant column: {name}"
                            )));
                        }
                        names.push(ci);
                    }
                    if names.len() != table.partition_key.len() {
                        let missing: Vec<&str> = table
                            .partition_key
                            .iter()
                            .filter(|p| !names.contains(p))
                            .map(|&p| table.columns[p].name.as_str())
                            .collect();
                        return Err(Error::invalid(format!(
                            "Local index definition must contain full partition key only. Missing column: {}",
                            missing.join(", ")
                        )));
                    }
                }
            }
        }
        if local && targets.len() != 1 {
            return Err(Error::invalid(
                "Local index definition must contain full partition key and exactly one indexed column",
            ));
        }
        if targets.is_empty() && !custom {
            return Err(Error::invalid(
                "Only CUSTOM indexes can be created without specifying a target column",
            ));
        }
        if targets.len() > 1 {
            if !custom || d.is_scylla() {
                return Err(Error::invalid(
                    "Only CUSTOM indexes support multiple columns",
                ));
            }
            return Err(Error::invalid(format!(
                "A storage-attached index cannot be created over multiple columns: {}",
                targets
                    .iter()
                    .map(|(ci, _, _)| table.columns[*ci].name.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            )));
        }
        (local, targets)
    };
    let (ci, mut kind, explicit) = targets[0];
    let col = table.columns[ci].clone();
    // A plain target on a collection means `values()` (Cassandra: frozen ones too, which then
    // fail the frozen check).
    if kind == TargetKind::Simple
        && col.ty.is_collection()
        && (col.ty.is_multicell() || !d.is_scylla())
    {
        kind = TargetKind::Values;
    }
    if local {
        if col.ty.is_collection() && col.ty.is_multicell() {
            return Err(Error::invalid(format!(
                "Local secondary index on collection column {} is not implemented yet.",
                col.name
            )));
        }
        if col.is_static() {
            return Err(Error::invalid(
                "Local indexes containing static columns are not supported.",
            ));
        }
    }
    // ScyllaDB's `enable_cassio_compatibility`: CassIO's SAI `ENTRIES` index on its metadata
    // map becomes a regular secondary index (`crate::guardrails::config_table`).
    let cassio = d.is_scylla()
        && matches!(class, Using::Sai(_))
        && kind == TargetKind::Entries
        && matches!(col.ty, CqlType::Map { .. })
        && db.settings().scylla.enable_cassio_compatibility;
    let mut cassio_warning = None;
    let class = if cassio {
        if !c.properties.is_empty() {
            return Err(Error::invalid(format!(
                "SAI index on map column {} cannot be rewritten as a regular secondary index for CassIO compatibility: regular secondary indexes don't support WITH OPTIONS",
                col.name
            )));
        }
        cassio_warning = Some(format!(
            "Creating a regular secondary index instead of the requested SAI index on map column {}: ScyllaDB supports SAI only on vector columns, and enable_cassio_compatibility rewrites CassIO's SAI ENTRIES index as a regular secondary index.",
            col.name
        ));
        Using::Default
    } else {
        class.clone()
    };
    let vector_index =
        matches!(class, Using::VectorIndex(_)) || (d.is_scylla() && matches!(class, Using::Sai(_)));
    let fulltext = matches!(class, Using::Fulltext(_));
    if fulltext {
        validate_fulltext(&table, ks.tablets.is_some(), &opts)?;
    } else if vector_index {
        validate_scylla_vector(&table, &opts)?;
    } else {
        crate::schema::compact::check_index(&table, ci)?;
        validate_target(d, &table, ci, kind, explicit)?;
    }
    if kind == TargetKind::Simple && col.ty.is_frozen_collection() && d.is_scylla() {
        kind = TargetKind::Full;
    }
    let name = match (&index_name, &vector_target) {
        (Some(n), _) => n.clone(),
        // A vector index with filtering columns is named after its table only.
        (None, Some(vt)) if !vt.fc.is_empty() => default_name_from(ks, &format!("{tname}_idx"), d),
        (None, _) => default_name(ks, &tname, &col.name, d),
    };
    // The stored definition.
    let (meta_kind, mut meta_opts) = match &class {
        Using::Default => ("COMPOSITES", BTreeMap::new()),
        Using::Sai(cls) if !d.is_scylla() => {
            let mut o = opts.clone();
            o.insert("class_name".into(), cls.clone());
            ("CUSTOM", o)
        }
        Using::Sasi => {
            let mut o = opts.clone();
            o.insert("class_name".into(), super::SASI_CLASS.into());
            ("CUSTOM", o)
        }
        Using::Sai(_) | Using::VectorIndex(_) | Using::Fulltext(_) => {
            let mut o = opts.clone();
            let cls = match &class {
                Using::VectorIndex(c) | Using::Fulltext(c) => c.clone(),
                _ => "vector_index".to_string(),
            };
            o.insert("class_name".into(), cls);
            o.insert(
                "index_version".into(),
                crate::uuid::format(&crate::uuid::timeuuid()),
            );
            ("CUSTOM", o)
        }
        Using::Unknown(cls) => {
            let mut o = opts.clone();
            o.insert("class_name".into(), cls.clone());
            ("CUSTOM", o)
        }
    };
    meta_opts.insert(
        "target".into(),
        match &vector_target {
            Some(vt) => vt.text(),
            None => target_text(d, &col, kind, local.then_some(&*table)),
        },
    );
    let meta = IndexMeta {
        name: name.clone(),
        kind: meta_kind.to_string(),
        options: meta_opts,
    };
    // `IndexMetadata.equalsWithoutName`: the same kind and options (ScyllaDB allows named vector
    // indexes on the same target, to replace one without downtime).
    if let Some(dup) = table
        .indexes
        .iter()
        .filter(|_| !(vector_index || fulltext) || index_name.is_none())
        .find(|i| i.kind == meta.kind && same_options(&i.options, &meta.options))
    {
        if c.if_not_exists {
            let mut w = tablets_warning();
            w.extend(class_warning());
            return Ok(Response::void().with_warnings(w));
        }
        return Err(Error::invalid(format!(
            "Index {name} is a duplicate of existing index {}",
            dup.name
        )));
    }
    // `IndexMetadata.validate`.
    let mut warnings: Vec<String> = cassio_warning.into_iter().collect();
    if !d.is_scylla() {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.is_empty() {
            return Err(Error::config(format!("Illegal index name {name}")));
        }
        if let Using::Unknown(cls) = &class {
            return Err(Error::config(format!(
                "Unable to find custom indexer class '{cls}'"
            )));
        }
    }
    let mut indexes = table.indexes.clone();
    indexes.push(meta.clone());
    let def = IndexDef {
        name: name.clone(),
        column: ci,
        target: kind,
        class: match &class {
            _ if local => IndexClass::Local,
            Using::Sai(_) if !d.is_scylla() => IndexClass::Sai,
            Using::Sasi => IndexClass::Sasi,
            Using::Sai(_) | Using::VectorIndex(_) => IndexClass::Vector,
            Using::Fulltext(_) => IndexClass::Fulltext,
            _ => IndexClass::Secondary,
        },
        options: meta.options.clone(),
    };
    if def.class == IndexClass::Sai {
        let custom = db.settings().sai_vector_allow_custom_parameters;
        validate_sai(&table, &def, &indexes, &opts, custom, &mut warnings)?;
    }
    if def.class == IndexClass::Sasi {
        super::sasi::validate(&table, ci, &opts, &raw_options(c))?;
        warnings.extend(class_warning());
    }
    if d.is_scylla() && !matches!(def.class, IndexClass::Vector | IndexClass::Fulltext) {
        warnings.extend(tablets_warning());
    }
    // The new schema: the base table with the index (and a new version), the index view.
    let mut base = table.rebuild(table.columns.clone(), table.params.clone());
    base.indexes = indexes;
    let base = Arc::new(base);
    let mut new_ks = (**ks).clone();
    new_ks.tables.insert(tname.clone(), base.clone());
    let mut attach = Vec::new();
    let mut view = None;
    if d.is_scylla() && def.has_table(&base) {
        let vname = format!("{name}_index");
        if ks.tables.contains_key(&vname) {
            return Err(Error::invalid(format!(
                "Index {name} cannot be created, because table {vname} already exists"
            )));
        }
        let t = super::index_table(
            &base,
            &def,
            &vname,
            view_id.unwrap_or_else(crate::uuid::timeuuid),
            d,
            ks.tablets.is_some(),
        );
        let t = match &view_props {
            Some(p) => apply_view_properties(t, p)?,
            None => t,
        };
        let t = Arc::new(t);
        new_ks.tables.insert(vname, t.clone());
        attach.push(t.clone());
        view = Some(t);
    }
    // ScyllaDB's Vector Store reads the CDC log: it comes with the first vector or full-text
    // index.
    if d.is_scylla() && matches!(def.class, IndexClass::Vector | IndexClass::Fulltext) {
        match new_ks.tables.get(&crate::cdc::log_name(&tname)).cloned() {
            Some(log) if crate::cdc::log_of(&new_ks, &tname).is_some() => {
                if let Some(l) = crate::cdc::follow_base(&log, &base, d, db.timestamp())? {
                    let l = Arc::new(l);
                    new_ks.tables.insert(l.name.clone(), l.clone());
                    attach.push(l);
                }
            }
            Some(_) => return Err(crate::cdc::log_name_taken(&ks_name, &tname)),
            None => {
                let log = Arc::new(crate::cdc::log_table(&base, d)?);
                new_ks.tables.insert(log.name.clone(), log.clone());
                attach.push(log);
            }
        }
    }
    let change = Change {
        keyspace: Some(new_ks),
        attach,
        events: vec![SchemaChange::object(
            ChangeType::Updated,
            ChangeTarget::Table,
            &ks_name,
            &tname,
        )],
        warnings,
        ..Default::default()
    };
    let r = commit(db, &schema, change).await?;
    // Backfill (after the writes that saw the old schema are applied). `commit` assigned the
    // tables' storage and attached the hidden index table: read them back.
    for o in db.hooks().observers {
        o.barrier().await;
    }
    let installed = db.schema();
    let base = installed.table(&ks_name, &tname).cloned().unwrap_or(base);
    let view = view.map(|v| installed.table(&ks_name, &v.name).cloned().unwrap_or(v));
    let spec = match view {
        Some(v) => Some(crate::views::ViewSpec::index_spec(&base, v, def, false)),
        None => super::hidden_table(&base, &def, d)
            .map(|h| crate::views::ViewSpec::index_spec(&base, Arc::new(h), def, true)),
    };
    if let Some(spec) = spec {
        crate::views::build(db, &spec, &base).await?;
    }
    Ok(r)
}

/// Options equal except ScyllaDB's `index_version` (a creation id).
fn same_options(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> bool {
    let strip = |m: &BTreeMap<String, String>| -> BTreeMap<String, String> {
        m.iter()
            .filter(|(k, _)| *k != "index_version")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    strip(a) == strip(b)
}

async fn drop_index(
    db: &Arc<Database>,
    session: &SessionState,
    x: &ast::DropObject<'static>,
    current: Option<&str>,
) -> Result<Response> {
    let d = db.dialect();
    let name = x.name.name.name().to_string();
    let ks_name = match (&x.name.keyspace, current) {
        (Some(k), _) => k.name().to_string(),
        (None, Some(k)) => k.to_string(),
        (None, None) => return Err(msg::no_keyspace()),
    };
    let schema = db.schema();
    if d.is_scylla() && schema.keyspace(&ks_name).is_none() {
        return Err(msg::keyspace_does_not_exist(d, &ks_name));
    }
    let found = schema.keyspace(&ks_name).and_then(|ks| {
        ks.tables
            .values()
            .find(|t| t.indexes.iter().any(|i| i.name == name))
            .cloned()
    });
    let Some(table) = found else {
        if x.if_exists {
            return Ok(Response::void());
        }
        return Err(match d {
            Dialect::Scylla => Error::invalid(format!(
                "Index '{name}' could not be found in any of the tables of keyspace '{ks_name}'"
            )),
            Dialect::Cassandra => {
                Error::invalid(format!("Index '{ks_name}.{name}' doesn't exist'"))
            }
        });
    };
    db.hooks().authorizer.authorize(
        db,
        session,
        &Access::Schema {
            keyspace: &ks_name,
            table: &table.name,
            statement: "DROP INDEX",
        },
    )?;
    let _g = db.ddl.lock().await;
    let schema = db.schema();
    let Some(ks) = schema.keyspace(&ks_name) else {
        return Err(msg::ddl_unknown_keyspace(d, &ks_name));
    };
    let Some(table) = ks.tables.get(&table.name).cloned() else {
        return Err(msg::unconfigured_table(d, &table.name));
    };
    let mut base = table.rebuild(table.columns.clone(), table.params.clone());
    base.indexes.retain(|i| i.name != name);
    let mut new_ks = (**ks).clone();
    new_ks.tables.insert(table.name.clone(), Arc::new(base));
    let mut drop_tables = Vec::new();
    let vname = format!("{name}_index");
    if let Some(v) = ks.tables.get(&vname)
        && v.view.as_ref().is_some_and(|m| m.base_table_id == table.id)
    {
        new_ks.tables.remove(&vname);
        drop_tables.push(v.table_id());
    }
    if !d.is_scylla() {
        drop_tables.push(super::hidden_storage_id(&table, &name));
    }
    let change = Change {
        keyspace: Some(new_ks),
        drop_tables,
        events: vec![SchemaChange::object(
            ChangeType::Updated,
            ChangeTarget::Table,
            &ks_name,
            &table.name,
        )],
        ..Default::default()
    };
    commit(db, &schema, change).await
}
