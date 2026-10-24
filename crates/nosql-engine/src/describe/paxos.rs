//! ScyllaDB's Paxos state tables (`t$paxos`). In a tablets keyspace ScyllaDB keeps a table's LWT
//! state in a table of its own, created by the first LWT operation on the base (a conditional
//! write or a SERIAL read). DESCRIBE shows it by name only, as an informational comment; it is
//! left out of DESCRIBE KEYSPACE and SCHEMA.
//!
//! roll keeps LWT state with the data, so only that table's existence (per base table, until the
//! database restarts) and its DESCRIBE are modeled.

use crate::database::Database;
use crate::describe::Options;
use crate::schema::{Column, ColumnKind, Keyspace, Table, TableFlags, TableKind, TableParams};
use nosql_types::{CqlType, Dialect};

/// The name suffix.
pub const SUFFIX: &str = "$paxos";

const NOTE: &str = "/* Do NOT execute this statement! It's only for informational purposes.\n   A paxos state table is created automatically when enabling LWT on a base table.\n\n";

/// Records an LWT operation on `t` (ScyllaDB, tablets keyspaces).
pub fn touch(db: &Database, t: &Table) {
    if db.dialect().is_scylla()
        && db
            .schema()
            .keyspace(&t.keyspace)
            .is_some_and(|k| k.tablets.is_some())
    {
        db.live.paxos_tables.lock().insert(t.id);
    }
}

/// The base table of `name` when it is a Paxos state table that exists.
pub fn base<'a>(db: &Database, k: &'a Keyspace, name: &str) -> Option<&'a Table> {
    let base = k.tables.get(name.strip_suffix(SUFFIX)?)?;
    db.live
        .paxos_tables
        .lock()
        .contains(&base.id)
        .then_some(&**base)
}

/// `DESCRIBE TABLE ks."t$paxos"`.
pub fn describe(k: &Keyspace, base: &Table, opts: Options) -> String {
    let col = |name: &str, ty: CqlType, kind: ColumnKind, position: i32, id: u32| Column {
        name: name.to_string(),
        ty,
        kind,
        position,
        reversed: false,
        id,
    };
    let columns = vec![
        col("row_key", CqlType::Blob, ColumnKind::PartitionKey, 0, 0),
        col(
            "most_recent_commit",
            CqlType::Blob,
            ColumnKind::Regular,
            -1,
            1,
        ),
        col(
            "most_recent_commit_at",
            CqlType::Timeuuid,
            ColumnKind::Regular,
            -1,
            2,
        ),
        col("promise", CqlType::Timeuuid, ColumnKind::Regular, -1, 3),
        col("proposal", CqlType::Blob, ColumnKind::Regular, -1, 4),
        col(
            "proposal_ballot",
            CqlType::Timeuuid,
            ColumnKind::Regular,
            -1,
            5,
        ),
    ];
    let mut params = TableParams::defaults(Dialect::Scylla, false);
    params.comment = format!("Paxos state for {}.{}", base.keyspace, base.name);
    params.gc_grace_seconds = 0;
    params.tombstone_gc.clear();
    params.extensions.clear();
    let t = Table::new(
        base.keyspace.clone(),
        format!("{}{SUFFIX}", base.name),
        base.id,
        base.version,
        columns,
        params,
        TableFlags {
            compound: true,
            dense: false,
            counter: false,
            super_: false,
        },
        TableKind::Regular,
    );
    format!("{NOTE}{}\n*/", super::scylla::table(k, &t, opts))
}
