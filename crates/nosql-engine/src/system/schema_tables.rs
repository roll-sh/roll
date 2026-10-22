//! The `system_schema` keyspace of each dialect: its tables' exact columns (as both references
//! list them in `system_schema.columns`). These are real storage tables holding the rows of user
//! keyspaces; the built-in keyspaces' rows are generated ([`crate::schema::persist`]).

use nosql_types::Dialect;

use super::{TableDef, c, def};
use crate::schema::TableKind;

/// The hidden column of `system_schema.tables` / `views` rows that keeps storage metadata (column
/// ids, the table version). Not part of the CQL definition, so never returned.
pub const HIDDEN_META: u32 = 10_000;
/// The hidden column of `system_schema.dropped_columns` rows with the dropped column's kind.
pub const HIDDEN_DROPPED_KIND: u32 = 10_001;

pub fn tables(d: Dialect) -> Vec<TableDef> {
    let ks = "system_schema";
    let gc = if d.is_scylla() { 864_000 } else { 604_800 };
    let mut v = vec![
        def(
            ks,
            "keyspaces",
            "keyspace definitions",
            gc,
            TableKind::SchemaTable,
            match d {
                Dialect::Scylla => vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("durable_writes", "boolean", 'r', -1),
                    c("next_replication", "frozen<map<text, text>>", 'r', -1),
                    c("replication", "frozen<map<text, text>>", 'r', -1),
                    c("replication_v2", "frozen<map<text, text>>", 'r', -1),
                ],
                Dialect::Cassandra => vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("durable_writes", "boolean", 'r', -1),
                    c("replication", "frozen<map<text, text>>", 'r', -1),
                ],
            },
        ),
        def(
            ks,
            "tables",
            "table definitions",
            gc,
            TableKind::SchemaTable,
            table_columns(d, false),
        ),
        def(
            ks,
            "columns",
            "column definitions",
            gc,
            TableKind::SchemaTable,
            column_columns(),
        ),
        def(
            ks,
            "dropped_columns",
            "dropped column registry",
            gc,
            TableKind::SchemaTable,
            {
                let mut cols = vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("table_name", "text", 'c', 0),
                    c("column_name", "text", 'c', 1),
                    c("dropped_time", "timestamp", 'r', -1),
                    c("type", "text", 'r', -1),
                ];
                if !d.is_scylla() {
                    cols.push(c("kind", "text", 'r', -1));
                }
                cols
            },
        ),
        def(
            ks,
            "triggers",
            "trigger definitions",
            gc,
            TableKind::SchemaTable,
            vec![
                c("keyspace_name", "text", 'p', 0),
                c("table_name", "text", 'c', 0),
                c("trigger_name", "text", 'c', 1),
                c("options", "frozen<map<text, text>>", 'r', -1),
            ],
        ),
        def(
            ks,
            "views",
            "view definitions",
            gc,
            TableKind::SchemaTable,
            table_columns(d, true),
        ),
        def(
            ks,
            "types",
            "user defined type definitions",
            gc,
            TableKind::SchemaTable,
            vec![
                c("keyspace_name", "text", 'p', 0),
                c("type_name", "text", 'c', 0),
                c("field_names", "frozen<list<text>>", 'r', -1),
                c("field_types", "frozen<list<text>>", 'r', -1),
            ],
        ),
        def(
            ks,
            "functions",
            "user defined function definitions",
            gc,
            TableKind::SchemaTable,
            vec![
                c("keyspace_name", "text", 'p', 0),
                c("function_name", "text", 'c', 0),
                c("argument_types", "frozen<list<text>>", 'c', 1),
                c("argument_names", "frozen<list<text>>", 'r', -1),
                c("body", "text", 'r', -1),
                c("called_on_null_input", "boolean", 'r', -1),
                c("language", "text", 'r', -1),
                c("return_type", "text", 'r', -1),
            ],
        ),
        def(
            ks,
            "aggregates",
            "user defined aggregate definitions",
            gc,
            TableKind::SchemaTable,
            vec![
                c("keyspace_name", "text", 'p', 0),
                c("aggregate_name", "text", 'c', 0),
                c("argument_types", "frozen<list<text>>", 'c', 1),
                c("final_func", "text", 'r', -1),
                c("initcond", "text", 'r', -1),
                c("return_type", "text", 'r', -1),
                c("state_func", "text", 'r', -1),
                c("state_type", "text", 'r', -1),
            ],
        ),
        def(
            ks,
            "indexes",
            "secondary index definitions",
            gc,
            TableKind::SchemaTable,
            vec![
                c("keyspace_name", "text", 'p', 0),
                c("table_name", "text", 'c', 0),
                c("index_name", "text", 'c', 1),
                c("kind", "text", 'r', -1),
                c("options", "frozen<map<text, text>>", 'r', -1),
            ],
        ),
    ];
    match d {
        Dialect::Scylla => {
            v.push(def(
                ks,
                "scylla_tables",
                "",
                gc,
                TableKind::SchemaTable,
                vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("table_name", "text", 'c', 0),
                    c("cdc", "frozen<map<text, text>>", 'r', -1),
                    c("committed_by_group0", "boolean", 'r', -1),
                    c("in_memory", "boolean", 'r', -1),
                    c("large_data_guardrails_enabled", "boolean", 'r', -1),
                    c("partitioner", "text", 'r', -1),
                    c("storage_engine", "text", 'r', -1),
                    c("tablets", "frozen<map<text, text>>", 'r', -1),
                    c("version", "uuid", 'r', -1),
                ],
            ));
            v.push(def(
                ks,
                "scylla_keyspaces",
                "scylla-specific information for keyspaces",
                gc,
                TableKind::SchemaTable,
                vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("consistency", "text", 'r', -1),
                    c("initial_tablets", "int", 'r', -1),
                    c("storage_options", "frozen<map<text, text>>", 'r', -1),
                    c("storage_type", "text", 'r', -1),
                ],
            ));
            v.push(def(
                ks,
                "scylla_aggregates",
                "scylla-specific information for user defined aggregates",
                gc,
                TableKind::SchemaTable,
                vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("aggregate_name", "text", 'c', 0),
                    c("argument_types", "frozen<list<text>>", 'c', 1),
                    c("reduce_func", "text", 'r', -1),
                    c("state_type", "text", 'r', -1),
                ],
            ));
            v.push(def(
                ks,
                "view_virtual_columns",
                "column definitions",
                gc,
                TableKind::SchemaTable,
                column_columns(),
            ));
            v.push(def(
                ks,
                "computed_columns",
                "computed columns",
                gc,
                TableKind::SchemaTable,
                vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("table_name", "text", 'c', 0),
                    c("column_name", "text", 'c', 1),
                    c("computation", "blob", 'r', -1),
                ],
            ));
        }
        Dialect::Cassandra => {
            v.push(def(
                ks,
                "column_masks",
                "column dynamic data masks",
                gc,
                TableKind::SchemaTable,
                vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("table_name", "text", 'c', 0),
                    c("column_name", "text", 'c', 1),
                    c("function_argument_nulls", "frozen<list<boolean>>", 'r', -1),
                    c("function_argument_types", "frozen<list<text>>", 'r', -1),
                    c("function_argument_values", "frozen<list<text>>", 'r', -1),
                    c("function_keyspace", "text", 'r', -1),
                    c("function_name", "text", 'r', -1),
                ],
            ));
        }
    }
    if !d.is_scylla() {
        for t in &mut v {
            t.memtable_flush_period_in_ms = 3_600_000;
        }
    }
    v
}

fn column_columns() -> Vec<super::ColDef> {
    vec![
        c("keyspace_name", "text", 'p', 0),
        c("table_name", "text", 'c', 0),
        c("column_name", "text", 'c', 1),
        c("clustering_order", "text", 'r', -1),
        c("column_name_bytes", "blob", 'r', -1),
        c("kind", "text", 'r', -1),
        c("position", "int", 'r', -1),
        c("type", "text", 'r', -1),
    ]
}

/// `system_schema.tables` (and `views`, which adds the base table and where clause).
fn table_columns(d: Dialect, view: bool) -> Vec<super::ColDef> {
    let mut v = vec![
        c("keyspace_name", "text", 'p', 0),
        c(
            if view { "view_name" } else { "table_name" },
            "text",
            'c',
            0,
        ),
    ];
    let common = [
        ("bloom_filter_fp_chance", "double"),
        ("caching", "frozen<map<text, text>>"),
        ("comment", "text"),
        ("compaction", "frozen<map<text, text>>"),
        ("compression", "frozen<map<text, text>>"),
        ("crc_check_chance", "double"),
        ("dclocal_read_repair_chance", "double"),
        ("default_time_to_live", "int"),
        ("extensions", "frozen<map<text, blob>>"),
        ("gc_grace_seconds", "int"),
        ("id", "uuid"),
        ("max_index_interval", "int"),
        ("memtable_flush_period_in_ms", "int"),
        ("min_index_interval", "int"),
        ("read_repair_chance", "double"),
        ("speculative_retry", "text"),
    ];
    for (n, t) in common {
        v.push(c(n, t, 'r', -1));
    }
    if !view {
        v.push(c("flags", "frozen<set<text>>", 'r', -1));
    } else {
        v.push(c("base_table_id", "uuid", 'r', -1));
        v.push(c("base_table_name", "text", 'r', -1));
        v.push(c("include_all_columns", "boolean", 'r', -1));
        v.push(c("where_clause", "text", 'r', -1));
    }
    if !d.is_scylla() {
        for (n, t) in [
            ("additional_write_policy", "text"),
            ("allow_auto_snapshot", "boolean"),
            ("cdc", "boolean"),
            ("incremental_backups", "boolean"),
            ("memtable", "text"),
            ("read_repair", "text"),
        ] {
            v.push(c(n, t, 'r', -1));
        }
    }
    v
}
