//! Deferred catalog scans use restricted generation and count without making whole rows.

mod common;

use common::{engine_session, open_store};
use pg_engine::catalog::system::{generated_rows_count, rel_rows_generated};
use pg_engine::types::Value;

#[test]
fn lazy_catalog_restricts_keys_and_counts_without_full_generation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "catalog_lazy_snapshot");
    let sql = include_str!("sql/a166_lazy_keys_count.sql");
    for stmt in common::statements(sql) {
        let stmt = stmt.trim();
        let restricted = stmt.starts_with("SELECT bool_or");
        let counted = stmt.starts_with("SELECT count(*) AS seen_old_count");
        let rel_before = rel_rows_generated();
        let whole_before = generated_rows_count();
        let result = s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
        if restricted {
            assert_eq!(result.last().unwrap().rows[0][0], Value::Bool(false));
            let n = rel_rows_generated() - rel_before;
            assert!(n > 0, "inner hash join did not generate restricted catalog rows");
            eprintln!("inner join restricted generations: {n}");
        }
        if counted {
            assert_eq!(result.last().unwrap().rows[0][0], Value::Int8(1));
            let n = generated_rows_count() - whole_before;
            assert_eq!(n, 0, "count(*) generated catalog rows instead of count_rest");
            // The engine retains a hashed SubPlan here, so this checks the count shortcut,
            // not catalog retention when DDL occurs before the count.
            eprintln!("count_rest whole catalog generations: {n}");
        }
    }
}
