//! The split views' rows of pg_catalog's and information_schema's own objects
//! (`system::split_view`), as PostgreSQL 18.6 returns them: exported from a fresh UTF8
//! database by `tools/export-split-views.sh` (data/split_rows), as the superuser and as a role
//! of no memberships. They depend on nothing the user's DDL changes, only on who reads them, so
//! the engine reads them instead of running the view's definition over the system objects on
//! a process's first query of each view and role (information_schema.columns' definition cost
//! more than 100 times a warm query; a loaded host's first query answered after a second).

use super::system::SysTable;
use crate::exec::Row;
use super::{FIRST_USER_OID, PG_DATABASE_OWNER_OID};
use crate::exec::ExecCtx;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// Which export's rows a reader sees.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Reader {
    /// A superuser: every row.
    Superuser,
    /// A role whose privileges on the system relations are PUBLIC's: no system relation's
    /// or column's ACL changed, a member of no predefined role (pg_read_all_data,
    /// pg_read_all_stats, ...) or of the bootstrap superuser. (The database owner's
    /// pg_database_owner counts as none: it owns only the public schema. These views check no
    /// schema's or database's privileges.)
    Plain,
}

/// Who `ctx` reads the split views' system rows as, when the exported rows are what PostgreSQL
/// returns them: in a UTF8 database (columns' octet lengths follow the encoding), with
/// pg_catalog first in the search path (after the temporary schema) or not named in it, so
/// the deparsed check clauses name its types unqualified.
pub fn reader(ctx: &ExecCtx) -> Option<Reader> {
    let utf8 = ctx
        .sess
        .cluster
        .database_locale(&ctx.sess.database)
        .is_none_or(|l| l.encoding == crate::types::collation::dblocale::UTF8);
    if !utf8 {
        return None;
    }
    if let Some(i) = ctx.search_path.iter().position(|s| s == "pg_catalog")
        && !ctx.search_path[..i].iter().all(|s| s.starts_with("pg_temp"))
    {
        return None;
    }
    let cat = ctx.catalog();
    let me = ctx.current_user();
    if cat.is_superuser(me) {
        return Some(Reader::Superuser);
    }
    let plain = me >= FIRST_USER_OID
        && !(cat.acls.iter()).any(|a| a.class == super::acl::ObjKind::Table.class_oid() && a.oid < FIRST_USER_OID)
        && (cat.roles_of(me, false, ctx.db_owner()).iter()).all(|&r| r >= FIRST_USER_OID || r == PG_DATABASE_OWNER_OID);
    plain.then_some(Reader::Plain)
}

fn source(table: SysTable) -> Option<&'static [u8]> {
    use SysTable::*;
    Some(match table {
        IsTables => include_bytes!("data/split_rows/tables.tsv.zst"),
        IsColumns => include_bytes!("data/split_rows/columns.tsv.zst"),
        IsTableConstraints => include_bytes!("data/split_rows/table_constraints.tsv.zst"),
        IsKeyColumnUsage => include_bytes!("data/split_rows/key_column_usage.tsv.zst"),
        IsConstraintColumnUsage => include_bytes!("data/split_rows/constraint_column_usage.tsv.zst"),
        IsCheckConstraints => include_bytes!("data/split_rows/check_constraints.tsv.zst"),
        PgTables => include_bytes!("data/split_rows/pg_tables.tsv.zst"),
        PgIndexes => include_bytes!("data/split_rows/pg_indexes.tsv.zst"),
        _ => return None,
    })
}

/// `table`'s rows of the system objects as `reader` sees them in database `db`: the
/// *_catalog columns `db`, pg_tables' owner `owner` (the bootstrap superuser's name). Parsed
/// each time (the caller keeps them), so nothing stays in memory for a process that never
/// reads them; a text repeated (a schema, a type, "NO") is one allocation.
pub fn rows(table: SysTable, reader: Reader, db: &str, owner: &str) -> Option<Vec<Row>> {
    let text = String::from_utf8(zstd::decode_all(source(table)?).expect("split view rows")).expect("split view rows");
    let cols = table.columns();
    let db = Value::Text(Arc::from(db));
    let owner = Value::Text(Arc::from(owner));
    let fmt = crate::types::text::FormatCtx::default();
    let mut texts: std::collections::HashMap<&str, Arc<str>> = std::collections::HashMap::new();
    let rows = text
        .lines()
        .filter_map(|l| {
            let (plain, l) = l.split_once('\t').expect("split view row");
            if reader == Reader::Plain && plain != "t" {
                return None;
            }
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), cols.len(), "{} row {l:?}", table.name());
            let row = f
                .iter()
                .zip(cols.iter())
                .map(|(x, (name, ty))| match (*x, name.as_str()) {
                    ("\\N", _) => Value::Null,
                    (_, "tableowner") => owner.clone(),
                    (_, n) if n.ends_with("_catalog") => db.clone(),
                    // (As `field` parses them, without COPY escapes to undo.)
                    (x, _) if matches!(ty, DataType::Name | DataType::Text | DataType::Varchar(_)) && !x.contains('\\') => {
                        Value::Text(texts.entry(x).or_insert_with(|| Arc::from(x)).clone())
                    }
                    _ => super::static_rows::field(x, ty, &fmt),
                })
                .collect();
            Some(row)
        })
        .collect();
    Some(rows)
}
