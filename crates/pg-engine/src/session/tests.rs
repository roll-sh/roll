use super::*;
use crate::session::state::SingleCluster;
use crate::sql::explain::{estimate, estimate_total};
use crate::storage::cache::BlockCache;
use crate::storage::{Store, StoreOptions};
use crate::txn::Database;

#[test]
fn estimate_total_matches_sql_plans_bitwise() {
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

            let queries = [
                (
                    "SELECT * FROM cost_rows WHERE id BETWEEN 20 AND 40 ORDER BY id",
                    "",
                ),
                ("SELECT v FROM cost_rows WHERE id = 42", ""),
                ("SELECT id + 1, v * 2 FROM cost_rows WHERE id < 20", ""),
                ("SELECT id FROM cost_rows ORDER BY v", "Sort"),
                (
                    "SELECT v, count(*) FROM cost_rows GROUP BY v ORDER BY v",
                    "",
                ),
                (
                    "SELECT v FROM (SELECT v FROM cost_rows ORDER BY v LIMIT 10) s WHERE v > 0",
                    "Subquery Scan",
                ),
                (
                    "SELECT a.id, (SELECT sum(b.v) FROM cost_rows b WHERE b.id < a.id)
                     FROM cost_rows a WHERE a.id < 5",
                    "SubPlan",
                ),
                (
                    "SELECT id FROM cost_rows WHERE v = (SELECT max(v) FROM cost_rows)",
                    "InitPlan",
                ),
                (
                    "SELECT (SELECT sum(v) FROM cost_rows
                     WHERE id < (SELECT max(id) FROM cost_rows))",
                    "InitPlan",
                ),
                ("SELECT * FROM pg_class", ""),
                (
                    "SELECT oid FROM pg_class WHERE oid > 0 ORDER BY relname LIMIT 5",
                    "",
                ),
                ("SELECT * FROM cost_rows", ""),
            ];
            let settings = [
                "RESET ALL",
                "SET max_parallel_workers_per_gather = 0",
                "SET max_parallel_workers_per_gather = 2;
                 SET min_parallel_table_scan_size = 0;
                 SET parallel_setup_cost = 0;
                 SET parallel_tuple_cost = 0;
                 SET debug_parallel_query = on",
                "SET max_parallel_workers_per_gather = 0;
                 SET enable_sort = off;
                 SET enable_hashagg = off;
                 SET enable_indexscan = off;
                 SET enable_bitmapscan = off",
            ];
            for (setting_index, settings) in settings.iter().enumerate() {
                session.query(settings).unwrap();
                for (sql, shape) in queries {
                    let prepared = session.parse(sql).unwrap().pop().unwrap();
                    let stmt = prepared.stmt.as_ref().unwrap();
                    let bound = session.bound(&prepared, stmt, &[]).unwrap();
                    // Both call orders exercise any state left by the richer extraction.
                    for total_first in [false, true] {
                        session
                            .with_ctx(&[], |ctx| {
                                let (total, full) = if total_first {
                                    let total = estimate_total(&bound.plan, ctx);
                                    (total, estimate(&bound.plan, ctx))
                                } else {
                                    let full = estimate(&bound.plan, ctx);
                                    (estimate_total(&bound.plan, ctx), full)
                                };
                                assert!(full.total.is_finite() && full.total > 0.0, "{sql}");
                                assert_eq!(
                                    total.to_bits(),
                                    full.total.to_bits(),
                                    "{sql}; {settings}; total_first={total_first}"
                                );
                                assert_eq!(
                                    plan_cost_estimate(ctx, &bound.plan, bound.hidden_rtes, false)
                                        .to_bits(),
                                    full.total.to_bits(),
                                    "session cost: {sql}; {settings}"
                                );
                                // One relation plus the statement itself: the custom planning charge.
                                if sql == "SELECT * FROM cost_rows" {
                                    assert_eq!(
                                        plan_cost_estimate(
                                            ctx,
                                            &bound.plan,
                                            bound.hidden_rtes,
                                            true
                                        )
                                        .to_bits(),
                                        (full.total + 5.0).to_bits(),
                                        "custom planning charge: {settings}"
                                    );
                                }
                                Ok(())
                            })
                            .unwrap();
                    }
                    session.end_batch().unwrap();
                    if setting_index == 0 && !shape.is_empty()
                        || setting_index == 2 && sql == "SELECT * FROM cost_rows"
                    {
                        let shape = if setting_index == 2 { "Gather" } else { shape };
                        let explained = session.query(&format!("EXPLAIN {sql}")).unwrap();
                        assert!(
                            explained
                                .iter()
                                .flat_map(|r| &r.rows)
                                .flatten()
                                .any(|v| matches!(v, Value::Text(line) if line.contains(shape))),
                            "expected {shape} for {sql}: {explained:?}"
                        );
                    }
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
