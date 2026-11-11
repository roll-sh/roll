//! A controlled build-first hash plan exercises the deferred catalog node contract.

mod common;

use common::{engine_session, open_store};
use pg_engine::catalog::system::{SysTable, generated_rows_count, rel_rows_generated, rows_cached};
use pg_engine::exec::{ExecCtx, collect};
use pg_engine::sql::binder::Binder;
use pg_engine::sql::plan::{JoinKind, JoinMethod, Plan};
use pg_engine::txn::Isolation;
use pg_engine::types::Value;
use std::cell::RefCell;
use std::sync::Arc;

fn build_first(plan: &mut Plan) {
    match plan {
        Plan::Project { input, .. } | Plan::Agg { input, .. } | Plan::Filter { input, .. } => {
            build_first(Arc::make_mut(input));
        }
        Plan::Join { kind, left, right, meta, .. } => {
            assert_eq!(*kind, JoinKind::Inner);
            assert_eq!(meta.method, Some(JoinMethod::Hash));
            assert!(matches!(**left, Plan::SysScan { table: SysTable::PgClass, .. }));
            assert!(pg_engine::sql::explain::plan_observable(right));
            assert!(meta.outer_first, "natural plan no longer prefetches the outer row");
            eprintln!("natural Hash Join outer_first=true; controlled clone outer_first=false");
            meta.outer_first = false;
        }
        _ => panic!("expected a direct catalog hash join"),
    }
}

#[test]
fn controlled_catalog_restriction_and_plain_count_generation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "catalog_lazy_snapshot");
    for stmt in common::statements(include_str!("sql/a166_lazy_keys_count.sql")) {
        let stmt = stmt.trim();
        if stmt.starts_with("SELECT bool_or") {
            let txn = RefCell::new(s.state.db.begin(Isolation::ReadCommitted, false, s.cancel.clone()));
            txn.borrow_mut().begin_statement(true).unwrap();
            let ctx = ExecCtx::new(&txn, &s.state, &[], 0, 0);
            let raw = pg_engine::sql::parse::parse(stmt).unwrap();
            let bound = Binder::new(&ctx, Vec::new()).bind_statement(raw[0].stmt.as_ref().unwrap()).unwrap();
            let mut controlled = bound.plan.clone();
            build_first(Arc::make_mut(&mut controlled));
            assert!(rows_cached(SysTable::PgClass, &ctx).is_none(), "controlled scan requires a cold catalog");
            let before = rel_rows_generated();
            let rows = collect(&controlled, &ctx, None).unwrap();
            assert_eq!(rows[0][0], Value::Bool(false));
            let n = rel_rows_generated() - before;
            assert!(n > 0, "controlled inner hash join did not generate restricted catalog rows");
            eprintln!("controlled inner join restricted generations: {n}");
            drop(ctx);
            txn.into_inner().commit().unwrap();
        } else {
            let before = generated_rows_count();
            let result = s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
            if stmt.starts_with("SELECT count(*) AS seen_old_count") {
                assert_eq!(result.last().unwrap().rows[0][0], Value::Int8(1));
                let n = generated_rows_count() - before;
                assert_eq!(n, 0, "plain count query generated a whole catalog");
                // The engine retains a hashed SubPlan here. Zero generation also permits
                // cached rows, so this does not prove count_rest or post-DDL retention.
                eprintln!("plain count query whole catalog generations: {n}");
            }
        }
    }
}
