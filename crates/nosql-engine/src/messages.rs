//! Every client-visible text that differs between the dialects, in one place.
//!
//! Each function returns the reference's exact message for the dialect. When a check exists in
//! only one product, the function returns `None` for the other (the statement then goes on). Add
//! new texts here rather than formatting them at the call site, with the reference's wording
//! verified on the image (see `docs/nosql/reference/`).

use nosql_types::Dialect;

use crate::error::{Error, ErrorCode};

type D = Dialect;

fn pick(d: D, scylla: impl Into<String>, cassandra: impl Into<String>) -> String {
    if d.is_scylla() {
        scylla.into()
    } else {
        cassandra.into()
    }
}

// ---- names and lookups -------------------------------------------------------------------------

pub fn multiple_property_definitions(d: D, name: &str) -> String {
    pick(
        d,
        format!("Multiple definition for property '{name}'"),
        format!("Multiple definitions for property '{name}'"),
    )
}

pub fn no_keyspace() -> Error {
    Error::invalid(
        "No keyspace has been specified. USE a keyspace, or explicitly specify keyspace.tablename",
    )
}

/// A statement on a missing keyspace (SELECT / modification / TRUNCATE).
pub fn keyspace_does_not_exist(d: D, ks: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Keyspace {ks} does not exist"),
        format!("keyspace {ks} does not exist"),
    ))
}

/// A statement on a missing table (SELECT / modification / TRUNCATE).
pub fn unconfigured_table(d: D, table: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("unconfigured table {table}"),
        format!("table {table} does not exist"),
    ))
}

/// `USE ks` on a missing keyspace.
pub fn use_unknown_keyspace(ks: &str) -> Error {
    Error::invalid(format!("Keyspace '{ks}' does not exist"))
}

/// A column name in a SELECT clause, WHERE clause, ORDER BY, …
pub fn undefined_column(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Unrecognized name {column}"),
        format!(
            "Undefined column name {} in table {ks}.{table}",
            nosql_types::quote_identifier(column, D::Cassandra)
        ),
    ))
}

/// A column name in INSERT / UPDATE / DELETE targets.
pub fn unknown_identifier(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Unknown identifier {column}"),
        format!(
            "Undefined column name {} in table {ks}.{table}",
            nosql_types::quote_identifier(column, D::Cassandra)
        ),
    ))
}

/// EXECUTE of an unknown or invalidated statement (same text in both references).
pub fn unprepared(d: D, id: &[u8]) -> Error {
    let mut e = Error::unprepared(id);
    if d == D::Scylla {
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        e.message = format!("No prepared statement with ID {hex} found.");
    }
    e
}

/// Rejected SASL credentials (shared by the server and the auth subsystem). ScyllaDB's wording
/// is not verified against the reference (it runs `AllowAllAuthenticator`).
pub fn bad_credentials(d: D, user: &str) -> Error {
    Error::new(
        ErrorCode::BadCredentials,
        match d {
            D::Scylla => "Username and/or password are incorrect".to_string(),
            D::Cassandra => format!("Provided username {user} and/or password are incorrect"),
        },
    )
}

/// roll-specific (neither reference has read-only databases): a write, schema, role, permission
/// or service level change on a read-only database (`Database::set_read_only`, read-only
/// branches). `UNAUTHORIZED`, the same text in both dialects.
pub fn read_only() -> Error {
    Error::unauthorized("This database is read-only")
}

// ---- DDL ---------------------------------------------------------------------------------------

pub fn keyspace_exists(ks: &str) -> Error {
    Error::already_exists(format!("Cannot add existing keyspace \"{ks}\""), ks, "")
}

pub fn table_exists(ks: &str, table: &str) -> Error {
    Error::already_exists(
        format!("Cannot add already existing table \"{table}\" to keyspace \"{ks}\""),
        ks,
        table,
    )
}

/// CREATE TABLE / TYPE / ALTER KEYSPACE in a missing keyspace.
pub fn ddl_unknown_keyspace(d: D, ks: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Can't find a keyspace {ks}"),
        format!("Keyspace '{ks}' doesn't exist"),
    ))
}

pub fn drop_unknown_keyspace(d: D, ks: &str) -> Error {
    match d {
        D::Scylla => Error::config(format!("Cannot drop non existing keyspace '{ks}'.")),
        D::Cassandra => Error::invalid(format!("Keyspace '{ks}' doesn't exist")),
    }
}

pub fn alter_unknown_table(d: D, ks: &str, table: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("unconfigured table {table}"),
        format!("Table '{ks}.{table}' doesn't exist"),
    ))
}

pub fn drop_unknown_table(d: D, ks: &str, table: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Cannot drop non existing table '{table}' in keyspace '{ks}'."),
        format!("Table '{ks}.{table}' doesn't exist"),
    ))
}

pub fn type_exists(d: D, ks: &str, name: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("A user type of name {ks}.{name} already exists"),
        format!("A user type with name '{name}' already exists"),
    ))
}

pub fn unknown_type_alter(d: D, ks: &str, name: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("No user type named {ks}.{name} exists."),
        format!("Type {ks}.{name} doesn't exist"),
    ))
}

pub fn unknown_type_drop(d: D, ks: &str, name: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("No user type named {ks}.{name} exists."),
        format!("Type '{ks}.{name}' doesn't exist"),
    ))
}

pub fn type_in_use(d: D, ks: &str, name: &str, tables: &[&str]) -> Error {
    Error::invalid(match d {
        D::Scylla => format!(
            "Cannot drop user type {ks}.{name} as it is still used by table {ks}.{}",
            tables.first().copied().unwrap_or_default()
        ),
        D::Cassandra => format!(
            "Cannot drop user type '{ks}.{name}' as it is still used by tables {}",
            tables.join(", ")
        ),
    })
}

pub fn keyspace_name_invalid(d: D, name: &str) -> Option<Error> {
    let valid_chars =
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    match d {
        D::Scylla if !valid_chars => Some(Error::invalid(format!(
            "\"{name}\" is not a valid keyspace name"
        ))),
        D::Scylla => (name.len() > 192).then(|| {
            Error::invalid(format!(
                "Keyspace names shouldn't be more than 192 characters long (got \"{name}\")"
            ))
        }),
        D::Cassandra => {
            if !valid_chars {
                Some(Error::invalid(format!(
                    "Keyspace name must not be empty and must contain alphanumeric or underscore characters only (got \"{name}\")"
                )))
            } else if name.len() > 48 {
                Some(Error::invalid(format!(
                    "Keyspace name must not be more than 48 characters long (got {} characters for \"{name}\")",
                    name.len()
                )))
            } else {
                None
            }
        }
    }
}

pub fn table_name_invalid(d: D, ks: &str, name: &str) -> Option<Error> {
    let valid_chars =
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    match d {
        D::Scylla => {
            if !valid_chars {
                Some(Error::invalid(format!(
                    "\"{name}\" is not a valid table name (must be alphanumeric character only: [0-9A-Za-z]+)"
                )))
            } else if name.len() > 192 {
                Some(Error::invalid(format!(
                    "Table names shouldn't be more than 192 characters long (got \"{name}\")"
                )))
            } else {
                None
            }
        }
        D::Cassandra => {
            if !valid_chars {
                Some(Error::config(format!(
                    "{ks}.{name}: Table name must not be empty or not contain non-alphanumeric-underscore characters (got \"{name}\")"
                )))
            } else if name.len() > 222 {
                Some(Error::config(format!(
                    "{ks}.{name}: Table name must not be more than 222 characters long (got {} characters for \"{name}\")",
                    name.len()
                )))
            } else {
                None
            }
        }
    }
}

pub fn no_primary_key(d: D, ks: &str, table: &str) -> Error {
    Error::invalid(pick(
        d,
        "No PRIMARY KEY specified (exactly one required)",
        format!("No PRIMARY KEY specifed for table '{ks}.{table}' (exactly one required)"),
    ))
}

pub fn multiple_primary_keys(d: D, ks: &str, table: &str) -> Error {
    Error::invalid(pick(
        d,
        "Multiple PRIMARY KEYs specified (exactly one required)",
        format!("Multiple PRIMARY KEY specified for table '{ks}.{table}' (exactly one required)"),
    ))
}

pub fn pk_unknown_column(d: D, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Unknown definition {column} referenced in PRIMARY KEY"),
        format!("Unknown column '{column}' referenced in PRIMARY KEY for table '{table}'"),
    ))
}

pub fn pk_duplicate_column(d: D, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Unknown definition {column} referenced in PRIMARY KEY"),
        format!("Duplicate column '{column}' in PRIMARY KEY clause for table '{table}'"),
    ))
}

pub fn duplicate_column_declaration(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Multiple definition of identifier {column}"),
        format!("Duplicate column '{column}' declaration for table '{ks}.{table}'"),
    ))
}

pub fn static_without_clustering() -> Error {
    Error::invalid(
        "Static columns are only useful (and thus allowed) if the table has at least one clustering column",
    )
}

pub fn static_in_primary_key(d: D, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Static column {column} cannot be part of the PRIMARY KEY"),
        format!("Static column '{column}' cannot be part of the PRIMARY KEY"),
    ))
}

pub fn clustering_order_not_clustering(d: D, columns: &[String]) -> Error {
    Error::invalid(pick(
        d,
        "Only clustering key columns can be defined in CLUSTERING ORDER directive".to_string(),
        format!(
            "Only clustering key columns can be defined in CLUSTERING ORDER directive: [{}] are not clustering columns",
            columns.join(", ")
        ),
    ))
}

pub fn clustering_order_wrong_order(d: D, a: &str, b: &str) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "The order of columns in the CLUSTERING ORDER directive must be the one of the clustering key ({a} must appear before {b})"
        ),
        format!(
            "The order of columns in the CLUSTERING ORDER directive must match that of the clustering columns ({a} must appear before {b})"
        ),
    ))
}

pub fn counter_mixed(d: D, column: &str) -> Error {
    match d {
        D::Scylla => Error::config(format!(
            "Cannot add a non counter column ({column}) in a counter column family"
        )),
        D::Cassandra => {
            Error::invalid("Cannot mix counter and non counter columns in the same table")
        }
    }
}

pub fn counter_with_ttl() -> Error {
    Error::invalid("Cannot set default_time_to_live on a table with counters")
}

pub fn compact_storage_disabled() -> Error {
    Error::invalid(
        "Support for the deprecated feature of 'CREATE TABLE WITH COMPACT STORAGE' is disabled and will eventually be removed in a future version.  To enable, set the 'enable_create_table_with_compact_storage' config option to 'true'.",
    )
}

pub fn alter_add_existing(d: D, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Invalid column name {column} because it conflicts with an existing column"),
        format!("Column with name '{column}' already exists"),
    ))
}

pub fn alter_add_counter_to_regular(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::config(pick(
        d,
        format!("Cannot add a counter column ({column}) in a non counter column family"),
        format!("{ks}.{table}: Cannot have a counter column (\"{column}\") in a non counter table"),
    ))
}

pub fn alter_add_regular_to_counter(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::config(pick(
        d,
        format!("Cannot add a non counter column ({column}) in a counter column family"),
        format!("{ks}.{table}: Cannot have a non counter column (\"{column}\") in a counter table"),
    ))
}

pub fn alter_drop_pk(d: D, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Cannot drop PRIMARY KEY part {column}"),
        format!("Cannot drop PRIMARY KEY column {column}"),
    ))
}

pub fn alter_drop_unknown(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Column {column} was not found in table {table}"),
        format!("Column {column} was not found in table '{ks}.{table}'"),
    ))
}

pub fn alter_rename_regular(d: D, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Cannot rename non PRIMARY KEY part {column}"),
        format!("Cannot rename non PRIMARY KEY column {column}"),
    ))
}

pub fn alter_rename_unknown(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Cannot rename unknown column {column} in table {table}"),
        format!("Column {column} was not found in table {ks}.{table}"),
    ))
}

pub fn alter_rename_to_existing(d: D, ks: &str, table: &str, from: &str, to: &str) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Cannot rename column {from} to {to} in table {table}; another column of that name already exist"
        ),
        format!(
            "Cannot rename column {from} to {to} in table '{ks}.{table}'; another column with that name already exists"
        ),
    ))
}

pub fn alter_column_type(d: D, column: &str, from: &str, to: &str) -> Error {
    match d {
        D::Scylla => Error::config(format!(
            "Cannot change {column} from type {from} to type {to}: types are incompatible."
        )),
        D::Cassandra => Error::invalid("Altering column types is no longer supported"),
    }
}

pub fn alter_type_add_existing(d: D, ks: &str, name: &str, field: &str) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Cannot add new field {field} to type {ks}.{name}: a field of the same name already exists"
        ),
        format!(
            "Cannot add field {field} to type {ks}.{name}: a field with name {field} already exists"
        ),
    ))
}

pub fn alter_type_unknown_field(d: D, ks: &str, name: &str, field: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Unknown field {field} in type {ks}.{name}"),
        format!("Unkown field {field} in user type {ks}.{name}"),
    ))
}

pub fn alter_type_field_type(
    d: D,
    ks: &str,
    name: &str,
    field: &str,
    from: &str,
    to: &str,
) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Type {to} in incompatible with previous type {from} of field {field} in user type {ks}.{name}"
        ),
        "Altering field types is no longer supported".to_string(),
    ))
}

// ---- writes -------------------------------------------------------------------------------------

pub fn missing_partition_key(d: D, columns: &[&str]) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Missing mandatory PRIMARY KEY part {}",
            columns.first().copied().unwrap_or_default()
        ),
        format!(
            "Some partition key parts are missing: {}",
            columns.join(", ")
        ),
    ))
}

pub fn missing_clustering_key(d: D, columns: &[&str]) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Missing mandatory PRIMARY KEY part {}",
            columns.first().copied().unwrap_or_default()
        ),
        format!("Some clustering keys are missing: {}", columns.join(", ")),
    ))
}

pub fn duplicate_insert_column(d: D, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Multiple definitions found for column {column}"),
        "The column names contains duplicates".to_string(),
    ))
}

pub fn update_pk_column(column: &str) -> Error {
    Error::invalid(format!("PRIMARY KEY part {column} found in SET part"))
}

pub fn non_pk_in_where(d: D, columns: &[&str]) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Invalid where clause contains non PRIMARY KEY columns: {}",
            columns.join(", ")
        ),
        format!(
            "Non PRIMARY KEY columns found in where clause: {} ",
            columns.join(", ")
        ),
    ))
}

pub fn multiple_setting(d: D, column: &str) -> Error {
    let m = format!("Multiple incompatible setting of column {column}");
    if d.is_scylla() {
        Error::invalid(m)
    } else {
        Error::syntax(m)
    }
}

pub fn counter_insert(d: D) -> Error {
    Error::invalid(pick(
        d,
        "INSERT statement are not allowed on counter tables, use UPDATE instead",
        "INSERT statements are not allowed on counter tables, use UPDATE instead",
    ))
}

pub fn non_counter_increment(d: D, column: &str, written: &str) -> Error {
    Error::invalid(pick(
        d,
        "SET with a column expression (e.g. col = col + 1) requires an LWT condition (e.g., IF col != NULL or IF EXISTS) to ensure atomic read-before-write".to_string(),
        format!("Invalid operation ({written}) for non counter column {column}"),
    ))
}

pub fn ttl_negative(d: D, ttl: i64) -> Error {
    Error::invalid(pick(
        d,
        "A TTL must be greater or equal to 0".to_string(),
        format!("A TTL must be greater or equal to 0, but was {ttl}"),
    ))
}

pub fn ttl_too_large(ttl: i64) -> Error {
    Error::invalid(format!(
        "ttl is too large. requested ({ttl}) maximum ({})",
        crate::limits::MAX_TTL
    ))
}

pub fn null_list_element(d: D) -> Error {
    Error::invalid(pick(
        d,
        "Invalid NULL element in list",
        "null is not supported inside collections",
    ))
}

pub fn list_literal_for_set(d: D, column: &str, ty: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Invalid list literal for {column} of type {ty}"),
        format!("Unexpected receiver type '{ty}'; only list and vector are expected"),
    ))
}

pub fn vector_wrong_dimension(d: D, column: &str, ty: &str, expected: usize, got: usize) -> Error {
    Error::invalid(pick(
        d,
        format!(
            "Invalid vector literal for {column}: type {ty} expects {expected} elements but got {got}"
        ),
        format!(
            "Invalid vector literal for {column} of type {ty}; expected {expected} elements, but given {got}"
        ),
    ))
}

pub fn invalid_bind_count(d: D, expected: usize, received: usize) -> Error {
    Error::invalid(pick(
        d,
        format!("Invalid amount of bind variables: expected {expected} received {received}"),
        "Invalid amount of bind variables".to_string(),
    ))
}

pub fn unset_routing_key(column: &str) -> Error {
    Error::invalid(format!("Invalid unset value for column {column}"))
}

pub fn batch_unlogged_multi_partition(n: usize, tables: &str) -> String {
    format!(
        "Unlogged batch covering {n} partitions detected against table [{tables}]. You should use a logged batch for atomicity, or asynchronous writes for performance."
    )
}

// ---- reads --------------------------------------------------------------------------------------

pub fn allow_filtering() -> Error {
    Error::invalid(
        "Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING",
    )
}

pub fn limit_not_positive(d: D, per_partition: bool) -> Error {
    Error::invalid(if per_partition && d.is_scylla() {
        "PER PARTITION LIMIT must be strictly positive"
    } else {
        "LIMIT must be strictly positive"
    })
}

pub fn order_by_in_paged() -> Error {
    Error::invalid(
        "Cannot page queries with both ORDER BY and a IN restriction on the partition key; you must either remove the ORDER BY or the IN and sort client side, or disable paging for this query",
    )
}

// ---- lightweight transactions --------------------------------------------------------------

/// An unknown column in an `IF` clause.
pub fn lwt_unknown_column(d: D, ks: &str, table: &str, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Unrecognized name {column}"),
        format!("Undefined column name {column} in table {ks}.{table}"),
    ))
}

/// `IF c[x]` on a column that is not a collection.
pub fn lwt_not_subscriptable(d: D, column: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("Column {column} is not a map/set/list, cannot be subscripted"),
        format!("Invalid element access syntax for non-collection column {column}"),
    ))
}

/// The bind variable of a list index in an `IF` clause.
pub fn lwt_list_index_name(d: D, column: &str) -> String {
    pick(d, format!("index({column})"), format!("idx({column})"))
}

/// `IF c.f` on a column that is not a UDT; Cassandra fails with a `ClassCastException`.
pub fn lwt_not_a_udt(d: D, column: &str, ty: &nosql_types::CqlType) -> Error {
    if d.is_scylla() {
        return Error::invalid(format!(
            "Invalid field selection: {column} of type {} is not a user type",
            nosql_types::cql_name(ty)
        ));
    }
    let marshal = nosql_types::marshal_name(ty, false, d);
    let class = marshal
        .split('(')
        .find(|c| !c.ends_with("FrozenType"))
        .unwrap_or(&marshal)
        .to_string();
    Error::new(
        ErrorCode::Server,
        format!(
            "java.lang.ClassCastException: class {class} cannot be cast to class org.apache.cassandra.db.marshal.UserType ({class} and org.apache.cassandra.db.marshal.UserType are in unnamed module of loader 'app')"
        ),
    )
}

/// `IF u.f` with an unknown field.
pub fn lwt_unknown_field(d: D, column: &str, udt: &str, field: &str) -> Error {
    Error::invalid(pick(
        d,
        format!("{column} of type {udt} has no field {field}"),
        format!("Unknown field {field} for column {column}"),
    ))
}

/// The bind variable of `IN ?` (`list`) or of one `IN (…)` item in an `IF` clause.
pub fn lwt_in_name(d: D, receiver: &str, negated: bool, item: bool) -> String {
    match (d, item) {
        (D::Scylla, false) => format!("{}({receiver})", if negated { "NOT IN" } else { "IN" }),
        (D::Scylla, true) => format!(
            "value({}({receiver}))",
            if negated { "NOT IN" } else { "IN" }
        ),
        (D::Cassandra, false) => format!("in({receiver})"),
        (D::Cassandra, true) => receiver.to_string(),
    }
}

/// An unset value in an `IF` clause.
pub fn lwt_unset_value(d: D, bind_index: usize) -> Error {
    Error::invalid(pick(
        d,
        format!("Unexpected unset value for bind variable {bind_index}"),
        "Invalid 'unset' value in condition",
    ))
}

pub fn lwt_batch_partitions(d: D) -> Error {
    Error::invalid(pick(
        d,
        "BATCH with conditions cannot span multiple partitions",
        "Batch with conditions cannot span multiple partitions",
    ))
}

pub fn lwt_batch_tables(d: D) -> Error {
    Error::invalid(pick(
        d,
        "BATCH with conditions cannot span multiple tables",
        "Batch with conditions cannot span multiple tables",
    ))
}

#[cfg(test)]
mod lwt_tests {
    use super::*;

    #[test]
    fn lwt_pairs() {
        assert_eq!(
            lwt_unknown_column(D::Scylla, "k", "t", "x").message,
            "Unrecognized name x"
        );
        assert_eq!(
            lwt_unknown_column(D::Cassandra, "k", "t", "x").message,
            "Undefined column name x in table k.t"
        );
        let e = lwt_not_a_udt(D::Cassandra, "v", &nosql_types::CqlType::Int);
        assert_eq!(e.code, ErrorCode::Server);
        assert_eq!(
            e.message,
            "java.lang.ClassCastException: class org.apache.cassandra.db.marshal.Int32Type cannot be cast to class org.apache.cassandra.db.marshal.UserType (org.apache.cassandra.db.marshal.Int32Type and org.apache.cassandra.db.marshal.UserType are in unnamed module of loader 'app')"
        );
        let l = nosql_types::CqlType::list(nosql_types::CqlType::Int, true);
        assert!(
            lwt_not_a_udt(D::Cassandra, "l", &l)
                .message
                .contains("class org.apache.cassandra.db.marshal.ListType cannot")
        );
        assert_eq!(lwt_in_name(D::Scylla, "v", true, true), "value(NOT IN(v))");
        assert_eq!(lwt_in_name(D::Cassandra, "v", false, false), "in(v)");
        assert_eq!(lwt_list_index_name(D::Scylla, "l"), "index(l)");
        assert_eq!(
            lwt_unset_value(D::Scylla, 2).message,
            "Unexpected unset value for bind variable 2"
        );
        assert_eq!(
            lwt_batch_tables(D::Cassandra).message,
            "Batch with conditions cannot span multiple tables"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_and_drop_table_texts() {
        let e = read_only();
        assert_eq!(
            (e.code, e.message.as_str()),
            (ErrorCode::Unauthorized, "This database is read-only")
        );
        assert_eq!(
            drop_unknown_table(D::Cassandra, "nope", "t").message,
            "Table 'nope.t' doesn't exist"
        );
        assert_eq!(
            drop_unknown_table(D::Scylla, "nope", "t").message,
            "Cannot drop non existing table 't' in keyspace 'nope'."
        );
    }

    #[test]
    fn name_validation() {
        assert!(keyspace_name_invalid(D::Scylla, "ok_1").is_none());
        assert_eq!(
            keyspace_name_invalid(D::Scylla, "bad-name")
                .unwrap()
                .message,
            "\"bad-name\" is not a valid keyspace name"
        );
        let long = "k".repeat(60);
        assert!(keyspace_name_invalid(D::Scylla, &long).is_none());
        assert_eq!(
            keyspace_name_invalid(D::Cassandra, &long).unwrap().message,
            format!(
                "Keyspace name must not be more than 48 characters long (got 60 characters for \"{long}\")"
            )
        );
        let t = "a".repeat(300);
        assert_eq!(
            table_name_invalid(D::Scylla, "e", &t).unwrap().message,
            format!("Table names shouldn't be more than 192 characters long (got \"{t}\")")
        );
        assert_eq!(
            table_name_invalid(D::Cassandra, "e", &t).unwrap().code,
            crate::ErrorCode::Config
        );
        assert!(table_name_invalid(D::Cassandra, "e", &"a".repeat(48)).is_none());
    }

    #[test]
    fn dialect_pairs() {
        assert_eq!(
            unconfigured_table(D::Scylla, "t").message,
            "unconfigured table t"
        );
        assert_eq!(
            unconfigured_table(D::Cassandra, "t").message,
            "table t does not exist"
        );
        assert_eq!(
            multiple_setting(D::Cassandra, "v").code,
            crate::ErrorCode::Syntax
        );
        assert_eq!(
            drop_unknown_keyspace(D::Scylla, "x").code,
            crate::ErrorCode::Config
        );
        assert_eq!(
            missing_partition_key(D::Cassandra, &["a", "b"]).message,
            "Some partition key parts are missing: a, b"
        );
        assert_eq!(
            missing_partition_key(D::Scylla, &["a", "b"]).message,
            "Missing mandatory PRIMARY KEY part a"
        );
    }
}
