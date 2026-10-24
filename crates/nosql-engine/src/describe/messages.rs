//! `DESCRIBE` error texts per dialect (verified against both references; see
//! `docs/nosql/reference/describe.md`).

use nosql_types::Dialect;

use crate::error::Error;

pub fn keyspace_not_found(ks: &str) -> Error {
    Error::invalid(format!("'{ks}' not found in keyspaces"))
}

pub fn no_keyspace() -> Error {
    Error::invalid("No keyspace specified and no current keyspace")
}

pub fn table_not_found(table: &str, ks: &str) -> Error {
    Error::invalid(format!("Table '{table}' not found in keyspace '{ks}'"))
}

/// ScyllaDB: `DESC TABLE` of a materialized view.
pub fn table_is_view() -> Error {
    Error::invalid(
        "Cannot use DESC TABLE on materialized View. (Did you mean DESC MATERIALIZED VIEW)?",
    )
}

pub fn index_table_not_found(index: &str, ks: &str) -> Error {
    Error::invalid(format!(
        "Table for existing index '{index}' not found in '{ks}'"
    ))
}

pub fn view_not_found(d: Dialect, view: &str, ks: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Materialized view '{view}' not found in keyspace '{ks}'"),
        Dialect::Cassandra => format!("Materialized view '{view}' not found in '{ks}'"),
    })
}

pub fn type_not_found(d: Dialect, ty: &str, ks: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Type '{ty}' not found in keyspace '{ks}'"),
        Dialect::Cassandra => format!("User defined type '{ty}' not found in '{ks}'"),
    })
}

pub fn function_not_found(d: Dialect, f: &str, ks: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Function '{f}' not found in keyspace '{ks}'"),
        Dialect::Cassandra => format!("User defined function '{f}' not found in '{ks}'"),
    })
}

pub fn aggregate_not_found(d: Dialect, a: &str, ks: &str) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => format!("Aggregate '{a}' not found in keyspace '{ks}'"),
        Dialect::Cassandra => format!("User defined aggregate '{a}' not found in '{ks}'"),
    })
}

/// `DESCRIBE name` that names nothing in the keyspace (same text in both).
pub fn not_found_in_keyspace(name: &str, ks: &str) -> Error {
    Error::invalid(format!("'{name}' not found in keyspace '{ks}'"))
}

/// ScyllaDB: `WITH PASSWORDS` outside `DESC SCHEMA`.
pub fn passwords_only_with_schema() -> Error {
    Error::invalid("Option WITH PASSWORDS is only allowed with DESC SCHEMA")
}

/// ScyllaDB: `WITH PASSWORDS` by a non-superuser (also without authentication).
pub fn passwords_need_superuser() -> Error {
    Error::unauthorized(
        "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS can only be issued by a superuser",
    )
}

/// Cassandra's `DescribeStatement.SCHEMA_CHANGED_WHILE_PAGING_MESSAGE`.
pub fn schema_changed_while_paging() -> Error {
    Error::invalid(
        "The schema has changed since the previous page of the DESCRIBE statement result. Please retry the DESCRIBE statement.",
    )
}

pub fn paging_release_mismatch(theirs: &str, ours: &str) -> Error {
    Error::invalid(format!(
        "The server version of the paging state {theirs} is different from the one of the server {ours}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_per_dialect() {
        let s = Dialect::Scylla;
        let c = Dialect::Cassandra;
        assert_eq!(
            type_not_found(s, "nope", "ref").message,
            "Type 'nope' not found in keyspace 'ref'"
        );
        assert_eq!(
            type_not_found(c, "nope", "ref").message,
            "User defined type 'nope' not found in 'ref'"
        );
        assert_eq!(
            view_not_found(c, "v", "ks").message,
            "Materialized view 'v' not found in 'ks'"
        );
        assert_eq!(
            function_not_found(c, "now", "system").message,
            "User defined function 'now' not found in 'system'"
        );
        assert_eq!(
            aggregate_not_found(s, "sum1", "ref").message,
            "Aggregate 'sum1' not found in keyspace 'ref'"
        );
        assert_eq!(
            passwords_need_superuser().code,
            crate::error::ErrorCode::Unauthorized
        );
        assert_eq!(
            keyspace_not_found("nope").message,
            "'nope' not found in keyspaces"
        );
    }
}
