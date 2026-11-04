
    fn size_failure_stats(s: &mut Session) -> Vec<i64> {
        s.query("SELECT pg_stat_force_next_flush()").unwrap();
        s.query("SELECT pg_stat_clear_snapshot()").unwrap();
        let result = s.query("SELECT n_tup_ins, n_tup_upd, n_tup_del, n_live_tup, n_dead_tup, n_ins_since_vacuum
            FROM pg_stat_user_tables WHERE relname='cs'").unwrap();
        assert_eq!(result[0].rows.len(), 1);
        result[0].rows[0].iter().map(|v| match v {
            Value::Int8(n) => *n,
            _ => panic!("expected int8 stats, got {v:?}"),
        }).collect()
    }

    // Expectations follow the accepted failed-INSERT accounting contract. These new
    // size-failure cases are unrun and need a separate PG18 capture before parity claims.
    #[test]
    fn combined_index_size_error_stats_unoracled() {
        for (len, message) in [(2800, "index row size 2816 exceeds"), (8300, "index row requires 8320 bytes")] {
            let dir = tempfile::tempdir().unwrap();
            let eng = Engine::open(EngineOptions { data_dir: dir.path().into(), sync: false, ..Default::default() }).unwrap();
            let mut s = eng.connect(1, "postgres", "postgres", "size-error-stats-test").unwrap();
            s.query("CREATE FUNCTION cs_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$
                SELECT left(string_agg(md5(g::text), '' ORDER BY g), n)
                FROM generate_series(1, (n+31)/32) g $$").unwrap();
            s.query("CREATE TABLE cs(id int PRIMARY KEY, payload text) WITH (autovacuum_enabled=off);
                CREATE INDEX cs_i ON cs(id) INCLUDE(payload);
                INSERT INTO cs VALUES (1, 'small')").unwrap();
            let before = size_failure_stats(&mut s);
            let error = s.query(&format!("INSERT INTO cs VALUES (2, cs_text({len}))")).unwrap_err();
            assert_eq!(error.code, "54000");
            assert!(error.message.starts_with(message), "{error:?}");
            let failed = size_failure_stats(&mut s);
            let delta: Vec<_> = failed.iter().zip(&before).map(|(a, b)| a - b).collect();
            eprintln!("UNRUN/UNORACLED size stats definition: len={len}, single failed INSERT expected delta={delta:?}, error={error:?}");
            assert_eq!(delta, vec![1, 0, 0, 0, 1, 1], "one failed heap insertion counts once");
            assert_eq!(s.query("SELECT count(*) FROM cs").unwrap()[0].rows, vec![vec![Value::Int8(1)]]);

            let error = s.query(&format!("INSERT INTO cs VALUES (2, 'small'), (3, cs_text({len}))")).unwrap_err();
            assert_eq!(error.code, "54000");
            assert!(error.message.starts_with(message), "{error:?}");
            let multi = size_failure_stats(&mut s);
            let delta: Vec<_> = multi.iter().zip(&failed).map(|(a, b)| a - b).collect();
            assert_eq!(delta, vec![2, 0, 0, 0, 2, 2], "earlier row and failed row each count once after statement rollback");
            assert_eq!(s.query("SELECT count(*) FROM cs").unwrap()[0].rows, vec![vec![Value::Int8(1)]]);

            s.query("INSERT INTO cs VALUES (2, 'small')").unwrap();
            let success = size_failure_stats(&mut s);
            let delta: Vec<_> = success.iter().zip(&multi).map(|(a, b)| a - b).collect();
            assert_eq!(delta, vec![1, 0, 0, 1, 0, 1], "successful insertion is not counted twice");
            for sql in [format!("UPDATE cs SET payload=cs_text({len}) WHERE id=1"),
                format!("INSERT INTO cs VALUES (1, cs_text({len})) ON CONFLICT(id) DO UPDATE SET payload=EXCLUDED.payload")] {
                let before_update = size_failure_stats(&mut s);
                let error = s.query(&sql).unwrap_err();
                assert_eq!(error.code, "54000");
                assert!(error.message.starts_with(message), "{error:?}");
                let after_update = size_failure_stats(&mut s);
                assert_eq!(after_update[0], before_update[0], "UPDATE must not add an INSERT count: {sql}");
                assert_eq!(after_update[5], before_update[5], "UPDATE must not add ins_since_vacuum: {sql}");
            }
        }
    }
