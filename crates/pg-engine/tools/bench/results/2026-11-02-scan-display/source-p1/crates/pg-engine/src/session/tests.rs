use super::*;
use crate::session::state::SingleCluster;
use crate::storage::cache::BlockCache;
use crate::storage::{Store, StoreOptions};
use crate::txn::Database;

#[test]
fn scalar_scan_display_preserves_estimates_and_subplans() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            crate::stack::set_limit(60 << 20);
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(
                dir.path(),
                StoreOptions {
                    memtable_bytes: 2 << 20,
                    cache: BlockCache::new(4 << 20),
                    sync: false,
                    ..StoreOptions::default()
                },
            )
            .unwrap();
            let db = Database::open("costs", 16384, store, "postgres").unwrap();
            let cluster = Arc::new(SingleCluster {
                name: "costs".into(),
                started: 0,
                locale: Default::default(),
                conf: Default::default(),
            });
            let mut session = Session::new(SessionState::new(db, cluster, "postgres", 4242));
            session
                .query(
                    "CREATE TABLE cost_rows (id int PRIMARY KEY, v int);
                     INSERT INTO cost_rows SELECT g, g % 17 FROM generate_series(1, 128) g;
                     ANALYZE cost_rows;",
                )
                .unwrap();

            session.query("CREATE FUNCTION cost_fn(int) RETURNS int LANGUAGE SQL STABLE AS 'SELECT max(v) FROM cost_rows WHERE id < $1'").unwrap();
            let queries = [
                "SELECT id, v FROM cost_rows WHERE id BETWEEN 20 AND 40 ORDER BY id",
                "SELECT id FROM cost_rows WHERE id BETWEEN $1 AND $1 + 9 ORDER BY id",
                "SELECT v FROM cost_rows WHERE id = 42",
                "SELECT id + 1 FROM cost_rows WHERE v > 2 AND v < 10",
                "SELECT id FROM cost_rows WHERE id > (SELECT max(v) FROM cost_rows)",
                "SELECT a.id FROM cost_rows a WHERE v > (SELECT sum(b.v) FROM cost_rows b WHERE b.id < a.id)",
                "SELECT id FROM cost_rows WHERE v > cost_fn(id)",
                "SELECT v, count(*) FROM cost_rows GROUP BY ROLLUP(v)",
                "SELECT (SELECT 1), GROUPING((SELECT 1)) FROM cost_rows GROUP BY ROLLUP((SELECT 1))",
                "SELECT a.id FROM cost_rows a WHERE EXISTS (SELECT 1 FROM cost_rows b WHERE b.id = a.id) OR a.v > 5",
                "SELECT id FROM cost_rows WHERE v::text COLLATE \"C\" > '5'",
                "SELECT v, count(*) FROM cost_rows GROUP BY v HAVING count(*) > 1",
                "SELECT a.id, b.v FROM cost_rows a JOIN cost_rows b ON b.id < a.id WHERE a.id < 4",
                "SELECT a.id FROM cost_rows a CROSS JOIN LATERAL (SELECT b.id FROM cost_rows b WHERE b.id < (SELECT max(c.v) FROM cost_rows c WHERE c.id < a.id) OFFSET 0) q WHERE a.id < 3",
                "SELECT id FROM cost_rows WHERE id IN (SELECT v FROM cost_rows WHERE id < 20)",
                "SELECT id FROM cost_rows WHERE id = 2 OR v = 4",
                "SELECT id, v FROM cost_rows ORDER BY id, v LIMIT 10",
            ];
            let settings = [
                "RESET ALL",
                "SET max_parallel_workers_per_gather = 0",
                "SET max_parallel_workers_per_gather = 2; SET min_parallel_table_scan_size = 0; SET min_parallel_index_scan_size = 0; SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0",
                "SET enable_seqscan = off; SET enable_indexscan = off; SET enable_indexonlyscan = off",
            ];
            let mut suppressed = 0;
            for settings in settings {
                session.query(settings).unwrap();
                for sql in queries {
                    let prepared = if sql.contains("$1") {
                        session.parse_message("", sql, &[crate::types::oid::INT4]).unwrap()
                    } else {
                        session.parse(sql).unwrap().pop().unwrap()
                    };
                    let bound = session.bound(&prepared, prepared.stmt.as_ref().unwrap(), &[]).unwrap();
                    session.with_ctx(&[], |ctx| {
                            let (bytes, _) = crate::sql::explain::assert_same_estimate(&bound.plan, ctx);
                            if sql.contains("BETWEEN") {
                                assert!(bytes > 0, "range conditions must exercise suppression: {settings}");
                            }
                            if sql.contains("cost_fn(id)") || sql.contains("COLLATE") {
                                assert_eq!(bytes, 0, "semantic wrappers must retain rendering: {sql}");
                            }
                            suppressed += bytes;
                        Ok(())
                    }).unwrap();
                    session.end_batch().unwrap();
                    let explained = session.query(&format!("EXPLAIN {}", sql.replace("$1", "20"))).unwrap();
                    if sql.contains("BETWEEN 20") {
                        assert!(explained.iter().flat_map(|r| &r.rows).flatten().any(|v| matches!(v, Value::Text(s) if s.contains("Index Cond:") || s.contains("Filter:"))));
                    }
                }
            }
            assert!(suppressed > 0, "the SQL plans must exercise condition suppression");

            session.query(
                "RESET ALL;
                 CREATE TABLE cost_parallel (id int PRIMARY KEY, v int);
                 INSERT INTO cost_parallel SELECT g, g % 17 FROM generate_series(1, 10000) g;
                 ANALYZE cost_parallel;
                 CREATE TABLE cost_parts (id int, v int) PARTITION BY RANGE (id);
                 CREATE TABLE cost_parts_a PARTITION OF cost_parts FOR VALUES FROM (0) TO (64);
                 CREATE TABLE cost_parts_b PARTITION OF cost_parts DEFAULT;
                 INSERT INTO cost_parts SELECT id, v FROM cost_rows;
                 ANALYZE cost_parts;"
            ).unwrap();
            let cases = [
                (
                    "RESET ALL; SET max_parallel_workers_per_gather=2; SET min_parallel_table_scan_size=0; SET min_parallel_index_scan_size=0; SET parallel_setup_cost=0; SET parallel_tuple_cost=0",
                    "SELECT sum(v) FROM cost_parallel WHERE v > 2",
                    "Parallel Seq Scan",
                ),
                (
                    "SET enable_seqscan=off; SET enable_bitmapscan=off",
                    "SELECT id, v FROM cost_parallel WHERE id > 10 ORDER BY id, v",
                    "Parallel Index Scan",
                ),
                (
                    "SET max_parallel_workers_per_gather=0",
                    "SELECT id FROM cost_parallel WHERE id > (SELECT max(v) FROM cost_rows)",
                    "Index Only Scan",
                ),
                (
                    "SET enable_indexscan=off; SET enable_indexonlyscan=off; SET enable_bitmapscan=on",
                    "SELECT id, v FROM cost_rows WHERE id BETWEEN 20 AND 40",
                    "Bitmap Heap Scan",
                ),
                (
                    "RESET ALL; SET enable_partitionwise_aggregate=on",
                    "SELECT v, count(*) FROM cost_parts WHERE id > 2 GROUP BY v",
                    "Append",
                ),
            ];
            for (settings, sql, required) in cases {
                session.query(settings).unwrap();
                let prepared = session.parse(sql).unwrap().pop().unwrap();
                let bound = session.bound(&prepared, prepared.stmt.as_ref().unwrap(), &[]).unwrap();
                session.with_ctx(&[], |ctx| {
                    let (_, node_types) = crate::sql::explain::assert_same_estimate(&bound.plan, ctx);
                    assert!(node_types.iter().any(|title| title.starts_with(required)), "missing {required}: {node_types:?}");
                    Ok(())
                }).unwrap();
                session.end_batch().unwrap();
                let explained = session.query(&format!("EXPLAIN {sql}")).unwrap();
                assert!(explained.iter().flat_map(|r| &r.rows).flatten().any(
                    |v| matches!(v, Value::Text(s) if s.contains(required))
                ), "displayed plan must exercise {required}");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
