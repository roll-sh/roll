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
        let proved = crate::sql::explain::no_gather_proof_applies(plan, ctx);
        if proved {
            let (during_recording, _) = crate::sql::literals::recording(|| {
                crate::sql::explain::no_gather_proof_applies(plan, ctx)
            });
            assert!(
                !during_recording,
                "dependency recording must use full discovery"
            );
        }
        (proved, crate::sql::explain::gathers(plan, ctx).len())
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
fn cheap_scan_proof_and_parallel_fallbacks() {
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
            assert!(
                !discovery(
                    &mut s,
                    "SELECT id,v FROM proof_rows WHERE id > (SELECT min(id) FROM proof_rows)",
                    &[]
                )
                .0
            );
            assert!(!discovery(&mut s, "SELECT id+1 FROM proof_rows WHERE id=20", &[]).0);
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
