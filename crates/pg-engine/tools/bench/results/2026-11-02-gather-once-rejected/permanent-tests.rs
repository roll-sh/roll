use super::*;

fn with_plan<T>(
    s: &mut Session,
    sql: &str,
    params: &[Value],
    f: impl FnOnce(&Arc<Plan>, &ExecCtx) -> T,
) -> T {
    let p = s
        .parse_message("proof", sql, if params.is_empty() { &[] } else { &[23] })
        .unwrap();
    let bq = s.planned(&p, p.stmt.as_ref().unwrap(), params).unwrap().0;
    let observed = s
        .with_ctx(params, |ctx| {
            crate::sql::binder::perms::lock_named_relations(&bq.perms, ctx)?;
            bq.check_perms(ctx)?;
            Ok(f(&bq.plan, ctx))
        })
        .unwrap();
    s.sync().unwrap();
    s.close_statement("proof");
    observed
}

fn discovery(s: &mut Session, sql: &str, params: &[Value]) -> (bool, usize) {
    with_plan(s, sql, params, |plan, ctx| {
        let (counts, gathers, original) = crate::sql::explain::gather_comparison(plan, ctx);
        if counts.proved > 0 {
            assert_eq!(counts.attempted, 1);
            assert_eq!(counts.serial, 1);
            assert_eq!(counts.relations, 1);
            assert_eq!(counts.partials, 0);
            assert!(gathers.is_empty());
        } else {
            assert_eq!(
                counts.serial, original.serial,
                "fallback repeated serial costing"
            );
            assert_eq!(counts.relations, original.relations);
            assert_eq!(counts.partials, original.partials);
        }
        println!("gather-counts sql={sql:?} optimized={counts:?} original={original:?}");
        (counts.proved == 1, gathers.len())
    })
}

fn serial_cost(s: &mut Session, sql: &str, params: &[Value]) -> f64 {
    with_plan(s, sql, params, |plan, ctx| {
        let mut p = plan;
        while let Plan::Project { input, .. } = &**p {
            p = input;
        }
        let Plan::Scan(scan) = &**p else {
            panic!("expected a scan")
        };
        crate::sql::paths::describe(ctx, scan).unwrap().total
    })
}

#[test]
fn gather_pruning_costs_once_and_preserves_fallbacks() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            crate::stack::set_limit(60 << 20);
            let dir = tempfile::tempdir().unwrap();
            let engine = crate::engine::Engine::open(crate::engine::EngineOptions {
                data_dir: dir.path().into(),
                sync: false,
                settings: vec![("autovacuum".into(), "off".into())],
                ..Default::default()
            })
            .unwrap();
            let mut s = engine
                .connect(2147000200, "postgres", "postgres", "gather-proof")
                .unwrap();
            s.query(
                "CREATE TABLE proof_rows(id int PRIMARY KEY, v int, note text);
            INSERT INTO proof_rows SELECT i, i % 100, 'text' FROM generate_series(1, 4000) i;
            CREATE INDEX proof_v ON proof_rows(v); ANALYZE proof_rows;
            SET min_parallel_table_scan_size=0; SET min_parallel_index_scan_size=0",
            )
            .unwrap();
            let ordered = "SELECT id, v FROM proof_rows WHERE id BETWEEN $1 AND $1+99 ORDER BY id";
            let unordered = "SELECT id, v FROM proof_rows WHERE id BETWEEN $1 AND $1+99";
            for mode in ["force_custom_plan", "force_generic_plan"] {
                s.query(&format!("SET plan_cache_mode={mode}")).unwrap();
                for sql in [ordered, unordered] {
                    assert_eq!(
                        discovery(&mut s, sql, &[Value::Int4(20)]),
                        (true, 0),
                        "{mode}: {sql}"
                    );
                }
            }
            s.query("SET parallel_tuple_cost=0").unwrap();
            for mode in ["force_custom_plan", "force_generic_plan"] {
                s.query(&format!("SET plan_cache_mode={mode}")).unwrap();
                for sql in [ordered, unordered] {
                    let total = serial_cost(&mut s, sql, &[Value::Int4(20)]);
                    assert!(total.is_finite() && total > 0.0);
                    for factor in [1.0, 1.0 + 1e-9, 1.005, 1.02] {
                        s.query(&format!("SET parallel_setup_cost={:.17}", total * factor))
                            .unwrap();
                        assert_eq!(
                            discovery(&mut s, sql, &[Value::Int4(20)]),
                            (factor > 1.0, 0),
                            "setup boundary: {total} * {factor}, {sql}"
                        );
                    }
                }
            }
            s.query("SET parallel_setup_cost=1; SET parallel_tuple_cost=0")
                .unwrap();
            for mode in ["force_custom_plan", "force_generic_plan"] {
                s.query(&format!("SET plan_cache_mode={mode}")).unwrap();
                for sql in [
                    ordered,
                    unordered,
                    "SELECT id,v FROM proof_rows WHERE id=$1",
                ] {
                    assert!(serial_cost(&mut s, sql, &[Value::Int4(20)]) > 1.0);
                    assert_eq!(discovery(&mut s, sql, &[Value::Int4(20)]), (false, 0));
                }
            }
            let actual = discovery(&mut s, "SELECT id,v FROM proof_rows", &[]);
            assert!(
                !actual.0 && actual.1 > 0,
                "positive setup real Gather: {actual:?}"
            );
            s.query("RESET parallel_setup_cost; RESET parallel_tuple_cost")
                .unwrap();
            s.query("SET enable_indexscan=off").unwrap();
            assert!(!discovery(&mut s, ordered, &[Value::Int4(20)]).0);
            s.query("RESET enable_indexscan; SET parallel_setup_cost=0; SET parallel_tuple_cost=0")
                .unwrap();
            let parallel = discovery(&mut s, "SELECT id,v FROM proof_rows", &[]);
            assert!(
                !parallel.0 && parallel.1 > 0,
                "real Gather must use full discovery: {parallel:?}"
            );
            let parallel_range = discovery(&mut s, ordered, &[Value::Int4(20)]);
            assert_eq!(
                parallel_range,
                (false, 0),
                "setup zero must decline even when the full planner still chooses serial"
            );
            s.query(
                "RESET parallel_setup_cost; RESET parallel_tuple_cost;
            CREATE INDEX proof_brin ON proof_rows USING brin(id)",
            )
            .unwrap();
            assert!(!discovery(&mut s, ordered, &[Value::Int4(20)]).0);
            s.query("DROP INDEX proof_brin").unwrap();
            for ddl in [
                "CREATE INDEX proof_extra ON proof_rows(v) WHERE v>5",
                "CREATE INDEX proof_extra ON proof_rows((id+1))",
            ] {
                s.query(ddl).unwrap();
                assert!(!discovery(&mut s, ordered, &[Value::Int4(20)]).0);
                s.query("DROP INDEX proof_extra").unwrap();
            }
            s.query("CREATE STATISTICS proof_ext ON id,v FROM proof_rows")
                .unwrap();
            assert!(!discovery(&mut s, ordered, &[Value::Int4(20)]).0);
            s.query("DROP STATISTICS proof_ext").unwrap();
            for sql in [
                "SELECT id FROM proof_rows WHERE id=20",
                "SELECT id FROM proof_rows WHERE id>20 ORDER BY id DESC",
                "SELECT v FROM proof_rows WHERE v>20",
                "SELECT id,v FROM proof_rows",
            ] {
                assert!(discovery(&mut s, sql, &[]).0, "admitted shape: {sql}");
            }
            with_plan(&mut s, ordered, &[Value::Int4(20)], |plan, ctx| {
                let ((counts, _, _), reads) = crate::sql::literals::recording(|| {
                    crate::sql::explain::gather_comparison(plan, ctx)
                });
                assert_eq!(counts.attempted, 0);
                assert_eq!(counts.proved, 0);
                assert!(!reads.is_empty());
            });
            assert!(
                !discovery(
                    &mut s,
                    "SELECT id,v FROM proof_rows WHERE id > (SELECT min(id) FROM proof_rows)",
                    &[]
                )
                .0
            );
            assert!(!discovery(&mut s, "SELECT id+1 FROM proof_rows WHERE id=20", &[]).0);
            with_plan(&mut s, "SELECT id,v FROM proof_rows WHERE id<0", &[], |plan,ctx| {
                let ((counts, _, _), reads) = crate::sql::literals::recording(|| crate::sql::explain::gather_comparison(plan,ctx));
                assert_eq!(counts.attempted,0);
                assert!(reads.iter().any(|r| matches!(r, crate::sql::literals::SizeRead::Data)), "histogram endpoint Data sentinel missing: {reads:?}");
            });
            let mut writer = engine.connect(2147000201, "postgres", "postgres", "gather-growth").unwrap();
            with_plan(&mut s, "SELECT id,v FROM proof_rows", &[], |plan,ctx| {
                let (before, _, _) = crate::sql::explain::gather_comparison(plan,ctx);
                assert_eq!(before.proved,1);
                writer.query("INSERT INTO proof_rows SELECT i, i%100, 'text' FROM generate_series(4001,104000) i").unwrap();
                let (after, _, _) = crate::sql::explain::gather_comparison(plan,ctx);
                assert_eq!(after.serial,1);
                assert_eq!(after.proved,0, "next operation must observe growth");
                assert_eq!(after.partials,1);
            });
            drop(writer);
            s.query(
                "SELECT pg_restore_attribute_stats('schemaname','public','relname','proof_rows',
            'attname','id','inherited',false,'correlation',5::real)",
            )
            .unwrap();
            assert!(
                !discovery(&mut s, ordered, &[Value::Int4(20)]).0,
                "invalid imported correlation"
            );
            drop(s);
            engine.shutdown();
        })
        .unwrap()
        .join()
        .unwrap();
}
