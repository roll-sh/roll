//! Logical replication between two engines in one process: the subscriber's native apply
//! (rows found by the replica identity, an index or the whole row; triggers by
//! session_replication_role; partition routing; conflicts and their statistics), streamed
//! transactions with rolled-back subtransactions, two-phase commit and disable_on_error. Checked against what PostgreSQL 18.6 subscribers do on the same
//! streams (HANDOFF "Logical replication").

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use pg_engine::session::Session;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn open(data: &Path, outbound: &[&str], settings: &[(&str, &str)]) -> Arc<Engine> {
    open_fsync(data, outbound, settings, false)
}

fn open_fsync(data: &Path, outbound: &[&str], settings: &[(&str, &str)], sync: bool) -> Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: data.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: outbound.iter().map(|s| s.to_string()).collect(),
        max_prepared_transactions: 4,
        auth_files: Default::default(),
        settings: settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    })
    .unwrap()
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last().unwrap().rows.iter().map(|r| format!("{r:?}")).collect()
}

/// `sql`'s rows on the subscriber once they equal `want` (the apply worker is asynchronous).
fn wait_for(s: &mut Session, sql: &str, want: &[String]) {
    let start = Instant::now();
    loop {
        let got = rows(s, sql);
        if got == want {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "{sql}: {got:?}, want {want:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Pair {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    _pub_engine: Arc<Engine>,
    _sub_engine: Arc<Engine>,
    p: Session,
    s: Session,
    port: u16,
}

fn pair() -> Pair {
    pair_with(&[])
}

/// A pair whose subscriber starts with `settings` (track_commit_timestamp, ...).
fn pair_with(settings: &[(&str, &str)]) -> Pair {
    pair_fsync(settings, false)
}

/// `pair_with`, the subscriber fsyncing its WAL (`sync`) or not.
fn pair_fsync(settings: &[(&str, &str)], sync: bool) -> Pair {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let publisher = open(d1.path(), &[], &[("wal_level", "logical"), ("logical_decoding_work_mem", "64kB")]);
    let port = pg_engine::server::spawn(publisher.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap().port();
    let subscriber = open_fsync(d2.path(), &["127.0.0.1"], settings, sync);
    let p = publisher.connect(1, "postgres", "postgres", "t").unwrap();
    let s = subscriber.connect(1, "postgres", "postgres", "t").unwrap();
    Pair { _dirs: (d1, d2), _pub_engine: publisher, _sub_engine: subscriber, p, s, port }
}

impl Pair {
    fn both(&mut self, sql: &str) {
        rows(&mut self.p, sql);
        rows(&mut self.s, sql);
    }

    fn subscribe(&mut self, name: &str, publication: &str, opts: &str) {
        let sql = format!(
            "CREATE SUBSCRIPTION {name} CONNECTION 'host=127.0.0.1 port={} user=postgres dbname=postgres' PUBLICATION {publication} WITH ({opts})",
            self.port
        );
        rows(&mut self.s, &sql);
    }

    /// The same rows on both sides.
    fn same(&mut self, sql: &str) {
        let want = rows(&mut self.p, sql);
        wait_for(&mut self.s, sql, &want);
    }
}

#[test]
fn native_apply() {
    let mut x = pair();
    x.both("CREATE TABLE pk (id int PRIMARY KEY, v text, n int, big text)");
    x.both("CREATE TABLE full_noidx (a int, b text)");
    x.both("CREATE TABLE ri (a int NOT NULL, b int NOT NULL, c text)");
    x.both("CREATE UNIQUE INDEX ri_ab ON ri (a, b)");
    x.both("ALTER TABLE ri REPLICA IDENTITY USING INDEX ri_ab");
    rows(&mut x.p, "ALTER TABLE full_noidx REPLICA IDENTITY FULL");
    rows(&mut x.p, "CREATE TABLE part (id int PRIMARY KEY, v text)");
    rows(&mut x.s, "CREATE TABLE part (id int PRIMARY KEY, v text) PARTITION BY RANGE (id)");
    rows(&mut x.s, "CREATE TABLE part1 PARTITION OF part FOR VALUES FROM (MINVALUE) TO (1000)");
    rows(&mut x.s, "CREATE TABLE part2 PARTITION OF part FOR VALUES FROM (1000) TO (MAXVALUE)");
    // Triggers: an ordinary one doesn't fire in apply, ALWAYS and REPLICA ones do.
    rows(&mut x.s, "CREATE TABLE log (op text, id int, v text)");
    rows(
        &mut x.s,
        "CREATE FUNCTION logf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         INSERT INTO public.log VALUES (TG_OP || ':' || TG_NAME, NEW.id, NEW.v); RETURN NEW; END $$",
    );
    rows(&mut x.s, "CREATE FUNCTION mark() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.v := NEW.v || '+r'; RETURN NEW; END $$");
    rows(&mut x.s, "CREATE TRIGGER t_plain AFTER INSERT ON pk FOR EACH ROW EXECUTE FUNCTION logf()");
    rows(&mut x.s, "CREATE TRIGGER t_always AFTER INSERT OR UPDATE ON pk FOR EACH ROW EXECUTE FUNCTION logf()");
    rows(&mut x.s, "ALTER TABLE pk ENABLE ALWAYS TRIGGER t_always");
    rows(&mut x.s, "CREATE TRIGGER t_replica BEFORE INSERT ON pk FOR EACH ROW EXECUTE FUNCTION mark()");
    rows(&mut x.s, "ALTER TABLE pk ENABLE REPLICA TRIGGER t_replica");
    rows(&mut x.p, "INSERT INTO pk SELECT g, 'v' || g, g, CASE WHEN g % 10 = 0 THEN repeat('x', 9000) END FROM generate_series(1, 50) g");
    rows(&mut x.p, "INSERT INTO full_noidx SELECT g % 10, 'b' || (g % 3) FROM generate_series(1, 40) g");
    rows(&mut x.p, "INSERT INTO ri SELECT g % 5, g, 'c' || g FROM generate_series(1, 30) g");
    rows(&mut x.p, "INSERT INTO part SELECT g * 10, 'p' || g FROM generate_series(1, 150) g");
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE pk, full_noidx, ri, part");
    x.subscribe("s", "p", "copy_data = true");
    wait_for(&mut x.s, "SELECT count(*) FROM pg_subscription_rel WHERE srsubstate <> 'r'", &rows(&mut x.p, "SELECT 0::bigint"));
    rows(&mut x.p, "UPDATE pk SET n = n + 1000 WHERE id % 3 = 0");
    rows(&mut x.p, "UPDATE pk SET id = id + 10000 WHERE id % 7 = 0");
    rows(&mut x.p, "DELETE FROM pk WHERE id % 11 = 0");
    rows(&mut x.p, "INSERT INTO pk VALUES (500, 'new', 1, NULL)");
    rows(&mut x.p, "UPDATE full_noidx SET b = b || '!' WHERE a = 3");
    rows(&mut x.p, "DELETE FROM full_noidx WHERE a = 4");
    rows(&mut x.p, "UPDATE ri SET b = b + 1000 WHERE a = 1");
    rows(&mut x.p, "DELETE FROM ri WHERE a = 2");
    rows(&mut x.p, "UPDATE part SET v = 'moved', id = id + 100000 WHERE id BETWEEN 500 AND 700");
    // A big transaction, streamed, with a subtransaction rolled back in it.
    rows(&mut x.p, "BEGIN; INSERT INTO pk SELECT g, 's', g, NULL FROM generate_series(1001, 2000) g; SAVEPOINT a; \
                    INSERT INTO pk SELECT g, 'rb', g, NULL FROM generate_series(2001, 3000) g; ROLLBACK TO a; COMMIT");
    rows(&mut x.p, "BEGIN; INSERT INTO pk SELECT g, 'abort', g, NULL FROM generate_series(5001, 7000) g; ROLLBACK");
    // (The replica trigger's mark on the subscriber's rows: compared without it.)
    x.same("SELECT id, replace(v, '+r', ''), n, md5(big) FROM pk ORDER BY id");
    x.same("SELECT a, b, count(*) FROM full_noidx GROUP BY a, b ORDER BY a, b");
    x.same("SELECT * FROM ri ORDER BY a, b");
    x.same("SELECT * FROM part ORDER BY id");
    assert_eq!(rows(&mut x.s, "SELECT v FROM pk WHERE id = 500"), vec!["[Text(\"new+r\")]".to_string()]);
    assert_eq!(rows(&mut x.s, "SELECT count(*) FROM log WHERE op LIKE '%t_plain'"), vec!["[Int8(0)]".to_string()]);
    let ups = rows(&mut x.s, "SELECT count(*) FROM log WHERE op = 'UPDATE:t_always'");
    assert_ne!(ups, vec!["[Int8(0)]".to_string()]);
    assert_eq!(rows(&mut x.s, "SELECT count(*) FROM part2 WHERE v = 'moved'"), vec!["[Int8(21)]".to_string()]);
    wait_for(&mut x.p, "SELECT stream_txns > 0 FROM pg_stat_replication_slots", &["[Bool(true)]".to_string()]);
}

#[test]
fn conflicts_and_disable_on_error() {
    let mut x = pair();
    x.both("CREATE TABLE t (id int PRIMARY KEY, v text)");
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE t");
    x.subscribe("s", "p", "copy_data = false, disable_on_error = true");
    rows(&mut x.p, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");
    x.same("SELECT * FROM t ORDER BY id");
    rows(&mut x.s, "DELETE FROM t WHERE id = 1; INSERT INTO t VALUES (3, 'local')");
    rows(&mut x.p, "UPDATE t SET v = 'u' WHERE id = 1");
    rows(&mut x.p, "DELETE FROM t WHERE id = 1");
    rows(&mut x.p, "INSERT INTO t VALUES (3, 'remote')");
    rows(&mut x.p, "INSERT INTO t VALUES (4, 'after')");
    // The duplicate key is an insert_exists conflict: the error disables the subscription.
    wait_for(&mut x.s, "SELECT subenabled FROM pg_subscription", &["[Bool(false)]".to_string()]);
    wait_for(
        &mut x.s,
        "SELECT apply_error_count, confl_insert_exists, confl_update_missing, confl_delete_missing FROM pg_stat_subscription_stats",
        &["[Int8(1), Int8(1), Int8(1), Int8(1)]".to_string()],
    );
    // The local row out of the way, enabled again: the rest applies.
    rows(&mut x.s, "DELETE FROM t WHERE id = 3");
    rows(&mut x.s, "ALTER SUBSCRIPTION s ENABLE");
    x.same("SELECT * FROM t ORDER BY id");
}

#[test]
fn two_phase() {
    let mut x = pair();
    x.both("CREATE TABLE t (id int PRIMARY KEY, v text)");
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE t");
    x.subscribe("s", "p", "copy_data = false, two_phase = true");
    wait_for(&mut x.s, "SELECT subtwophasestate FROM pg_subscription", &["[Char(101)]".to_string()]);
    rows(&mut x.p, "BEGIN; INSERT INTO t VALUES (1, 'a'); PREPARE TRANSACTION 'g1'");
    wait_for(&mut x.s, "SELECT count(*) FROM pg_prepared_xacts", &["[Int8(1)]".to_string()]);
    assert_eq!(rows(&mut x.s, "SELECT count(*) FROM t"), vec!["[Int8(0)]".to_string()]);
    rows(&mut x.p, "COMMIT PREPARED 'g1'");
    rows(&mut x.p, "BEGIN; INSERT INTO t SELECT g, 'big' FROM generate_series(10, 3000) g; PREPARE TRANSACTION 'g2'");
    wait_for(&mut x.s, "SELECT count(*) FROM pg_prepared_xacts", &["[Int8(1)]".to_string()]);
    rows(&mut x.p, "ROLLBACK PREPARED 'g2'");
    rows(&mut x.p, "INSERT INTO t VALUES (2, 'b')");
    x.same("SELECT * FROM t ORDER BY id");
    wait_for(&mut x.s, "SELECT count(*) FROM pg_prepared_xacts", &["[Int8(0)]".to_string()]);
}

/// binary = true: the initial copy in binary COPY format and the stream's values in their
/// types' send formats, both ways of every common type.
#[test]
fn binary_transfer() {
    let mut x = pair();
    let ddl = "CREATE TABLE b (id int PRIMARY KEY, n numeric(10,2), t text, ts timestamptz, j jsonb, a int[], \
               by bytea, u uuid, f float8, vc varchar(5), iv interval, d date, bo bool, big bigint, tt text)";
    x.both(ddl);
    let ins = |lo: i32, hi: i32| {
        format!(
            "INSERT INTO b SELECT g, g * 1.25, 't' || g, '2026-01-01 00:00:00+00'::timestamptz + g * interval '1 hour', \
             jsonb_build_object('k', g, 'a', ARRAY[g, -g]), ARRAY[g, NULL, g + 1], decode(md5(g::text), 'hex'), \
             md5(g::text)::uuid, g / 7.0, left('v' || g, 5), g * interval '1 day 3 sec', DATE '2000-01-01' + g, g % 2 = 0, \
             g::bigint << 40, CASE WHEN g % 10 = 0 THEN repeat('x', 5000) END FROM generate_series({lo}, {hi}) g"
        )
    };
    rows(&mut x.p, &ins(1, 300));
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE b");
    x.subscribe("s", "p", "copy_data = true, binary = true");
    wait_for(&mut x.s, "SELECT count(*) FROM pg_subscription_rel WHERE srsubstate <> 'r'", &rows(&mut x.p, "SELECT 0::bigint"));
    rows(&mut x.p, &ins(301, 400));
    rows(&mut x.p, "UPDATE b SET n = -n, j = j || '{\"u\": true}', f = 'NaN' WHERE id % 3 = 0");
    rows(&mut x.p, "UPDATE b SET id = id + 1000, a = NULL WHERE id % 7 = 0");
    rows(&mut x.p, "DELETE FROM b WHERE id % 11 = 0");
    x.same("SELECT id, n, t, ts, j, a, by, u, f, vc, iv, d, bo, big, md5(tt) FROM b ORDER BY id");
}

/// track_commit_timestamp on the subscriber: applied rows carry the remote commit time and
/// the subscription's origin; changing a row changed locally logs update_origin_differs /
/// delete_origin_differs (and applies).
#[test]
fn commit_timestamps_and_origin_differs() {
    let mut x = pair_with(&[("track_commit_timestamp", "on")]);
    x.both("CREATE TABLE t (id int PRIMARY KEY, v text)");
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE t");
    x.subscribe("s", "p", "copy_data = false");
    rows(&mut x.p, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");
    x.same("SELECT * FROM t ORDER BY id");
    let origin = rows(&mut x.s, "SELECT roident FROM pg_replication_origin");
    assert_eq!(rows(&mut x.s, "SELECT (pg_xact_commit_timestamp_origin(xmin)).roident FROM t WHERE id = 3"), origin);
    assert_eq!(
        rows(&mut x.s, "SELECT pg_xact_commit_timestamp(xmin) IS NOT NULL, (pg_last_committed_xact()).xid IS NOT NULL FROM t WHERE id = 1"),
        vec!["[Bool(true), Bool(true)]".to_string()]
    );
    rows(&mut x.s, "UPDATE t SET v = 'local' WHERE id IN (1, 2)");
    assert_eq!(
        rows(&mut x.s, "SELECT (pg_xact_commit_timestamp_origin(xmin)).roident FROM t WHERE id = 1"),
        vec!["[Oid(0)]".to_string()]
    );
    rows(&mut x.p, "UPDATE t SET v = 'remote' WHERE id = 1");
    rows(&mut x.p, "DELETE FROM t WHERE id = 2");
    rows(&mut x.p, "UPDATE t SET v = 'same' WHERE id = 3");
    x.same("SELECT * FROM t ORDER BY id");
    wait_for(
        &mut x.s,
        "SELECT confl_update_origin_differs, confl_delete_origin_differs FROM pg_stat_subscription_stats",
        &["[Int8(1), Int8(1)]".to_string()],
    );
    let e = x.s.query("SELECT pg_xact_commit_timestamp('0'::xid)").unwrap_err();
    assert_eq!(e.message.as_str(), "cannot retrieve commit timestamp for transaction 0");
    assert_eq!(rows(&mut x.s, "SELECT pg_xact_commit_timestamp('2'::xid)"), vec!["[Null]".to_string()]);
    // Off (the publisher): normal ids are an error, the special ones still answer.
    let e = x.p.query("SELECT pg_last_committed_xact()").unwrap_err();
    assert_eq!(e.message.as_str(), "could not get commit timestamp data");
    assert_eq!(rows(&mut x.p, "SELECT pg_xact_commit_timestamp('1'::xid)"), vec!["[Null]".to_string()]);
}

/// run_as_owner = false: changes run as the table's owner (triggers see it as current_user);
/// the target's privileges and row-level security are checked (TargetPrivilegesCheck).
#[test]
fn run_as_owner_and_privileges() {
    let mut x = pair();
    x.both("CREATE TABLE t (id int PRIMARY KEY, v text)");
    x.both("CREATE TABLE r (id int PRIMARY KEY, v text)");
    rows(&mut x.s, "CREATE ROLE app; ALTER TABLE t OWNER TO app; ALTER TABLE r OWNER TO app");
    rows(&mut x.s, "CREATE TABLE who (u text)");
    rows(&mut x.s, "GRANT INSERT ON who TO app");
    rows(
        &mut x.s,
        "CREATE FUNCTION whof() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         INSERT INTO public.who VALUES (current_user); RETURN NEW; END $$",
    );
    rows(&mut x.s, "CREATE TRIGGER tw AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION whof()");
    rows(&mut x.s, "ALTER TABLE t ENABLE ALWAYS TRIGGER tw");
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE t");
    rows(&mut x.p, "CREATE PUBLICATION pr FOR TABLE r");
    x.subscribe("s", "p", "copy_data = false");
    rows(&mut x.p, "INSERT INTO t VALUES (1, 'a')");
    x.same("SELECT * FROM t ORDER BY id");
    assert_eq!(rows(&mut x.s, "SELECT u FROM who"), vec!["[Text(\"app\")]".to_string()]);
    // Row-level security the owner is subject to: apply refuses the table.
    rows(&mut x.s, "ALTER TABLE r ENABLE ROW LEVEL SECURITY; ALTER TABLE r FORCE ROW LEVEL SECURITY");
    x.subscribe("sr", "pr", "copy_data = false");
    rows(&mut x.p, "INSERT INTO r VALUES (1, 'a')");
    wait_for(
        &mut x.s,
        "SELECT apply_error_count > 0 FROM pg_stat_subscription_stats WHERE subname = 'sr'",
        &["[Bool(true)]".to_string()],
    );
    assert_eq!(rows(&mut x.s, "SELECT count(*) FROM r"), vec!["[Int8(0)]".to_string()]);
    rows(&mut x.s, "ALTER TABLE r NO FORCE ROW LEVEL SECURITY");
    x.same("SELECT * FROM r ORDER BY id");
}

/// Publication row filters and column lists end to end: the initial copy takes only the rows
/// the filters pass (through COPY (SELECT ...)) and the listed columns, the stream likewise.
#[test]
fn row_filters_and_column_lists() {
    let mut x = pair();
    x.both("CREATE TABLE f (id int PRIMARY KEY, v text, secret text, n int)");
    rows(&mut x.p, "INSERT INTO f SELECT g, 'v' || g, 's' || g, g FROM generate_series(1, 100) g");
    rows(&mut x.p, "CREATE PUBLICATION pf FOR TABLE f (id, v, n) WHERE (id % 2 = 0)");
    x.subscribe("s", "pf", "copy_data = true");
    wait_for(&mut x.s, "SELECT count(*) FROM pg_subscription_rel WHERE srsubstate <> 'r'", &rows(&mut x.p, "SELECT 0::bigint"));
    rows(&mut x.p, "INSERT INTO f VALUES (101, 'odd', 'x', 101), (102, 'even', 'x', 102)");
    rows(&mut x.p, "UPDATE f SET v = v || '!', n = n + 1 WHERE id IN (3, 4)");
    rows(&mut x.p, "DELETE FROM f WHERE id IN (6, 7)");
    x.same("SELECT id, v, NULL::text, n FROM f WHERE id % 2 = 0 ORDER BY id");
    assert_eq!(rows(&mut x.s, "SELECT count(*) FROM f WHERE secret IS NOT NULL"), vec!["[Int8(0)]".to_string()]);
}

/// streaming = parallel: streamed transactions go to a parallel apply worker (its
/// pg_stat_subscription row, a savepoint rolled back, an aborted transaction, a streamed
/// PREPARE), and with debug_logical_replication_streaming = immediate the leader serializes
/// them to a file the worker applies at their end.
#[test]
fn parallel_apply() {
    for immediate in [false, true] {
        let settings: &[(&str, &str)] =
            if immediate { &[("debug_logical_replication_streaming", "immediate")] } else { &[] };
        let mut x = pair_with(settings);
        x.both("CREATE TABLE t (id int PRIMARY KEY, v text)");
        rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE t");
        x.subscribe("s", "p", "copy_data = false, streaming = parallel, two_phase = true");
        wait_for(&mut x.s, "SELECT subtwophasestate FROM pg_subscription", &["[Char(101)]".to_string()]);
        rows(&mut x.p, "INSERT INTO t VALUES (0, 'small')");
        rows(
            &mut x.p,
            "BEGIN; INSERT INTO t SELECT g, 's' FROM generate_series(1, 2000) g; SAVEPOINT a; \
             INSERT INTO t SELECT g, 'rb' FROM generate_series(2001, 3000) g; ROLLBACK TO a; \
             UPDATE t SET v = 'u' WHERE id % 7 = 0; COMMIT",
        );
        rows(&mut x.p, "BEGIN; INSERT INTO t SELECT g, 'abort' FROM generate_series(5001, 7000) g; ROLLBACK");
        rows(&mut x.p, "INSERT INTO t VALUES (9000, 'after')");
        x.same("SELECT id, v FROM t ORDER BY id");
        // The worker stays in the pool (max_parallel_apply_workers_per_subscription = 2),
        // unless its transaction was serialized (pa_free_worker stops it then).
        let mut want = vec!["[Text(\"apply\"), Null, Bool(true), Bool(false)]".to_string()];
        if !immediate {
            want.push("[Text(\"parallel apply\"), Bool(true), Bool(true), Bool(true)]".to_string());
        }
        wait_for(
            &mut x.s,
            "SELECT worker_type, leader_pid = (SELECT pid FROM pg_stat_subscription WHERE worker_type = 'apply'), \
             relid IS NULL, received_lsn IS NULL FROM pg_stat_subscription ORDER BY worker_type",
            &want,
        );
        rows(&mut x.p, "BEGIN; INSERT INTO t SELECT g, 'p' FROM generate_series(10001, 12000) g; PREPARE TRANSACTION 'g1'");
        wait_for(&mut x.s, "SELECT count(*) FROM pg_prepared_xacts", &["[Int8(1)]".to_string()]);
        rows(&mut x.p, "COMMIT PREPARED 'g1'");
        x.same("SELECT count(*), sum(id) FROM t");
        wait_for(&mut x.s, "SELECT count(*) FROM pg_prepared_xacts", &["[Int8(0)]".to_string()]);
        rows(&mut x.s, "DROP SUBSCRIPTION s");
    }
}

/// synchronous_commit = off (a subscription's default): applied transactions commit without
/// waiting for the WAL fsync, and the publisher hears them flushed only once they are
/// durable (send_feedback's get_flush_position), so a crash before that resends them.
#[test]
fn asynchronous_apply_confirms_durable_only() {
    let mut x = pair_fsync(&[("wal_writer_delay", "10s")], true);
    x.both("CREATE TABLE t (id int PRIMARY KEY, v text)");
    rows(&mut x.p, "CREATE PUBLICATION p FOR TABLE t");
    x.subscribe("s", "p", "copy_data = false");
    let flushed = "SELECT confirmed_flush_lsn >= pg_current_wal_lsn() FROM pg_replication_slots WHERE slot_name = 's'";
    rows(&mut x.p, "INSERT INTO t VALUES (0, 'warm-up')");
    x.same("SELECT * FROM t ORDER BY id");
    // A synchronous commit flushes what came before; then the WAL writer finds nothing left
    // and stops, so the next asynchronous commit waits its whole wal_writer_delay.
    rows(&mut x.s, "CREATE TABLE local_only ()");
    let yes = rows(&mut x.p, "SELECT true");
    wait_for(&mut x.p, flushed, &yes);
    std::thread::sleep(Duration::from_millis(10_500));
    rows(&mut x.p, "INSERT INTO t SELECT g, 'v' || g FROM generate_series(1, 100) g");
    x.same("SELECT count(*) FROM t");
    // Applied and visible, not yet durable: not confirmed.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(rows(&mut x.p, flushed), rows(&mut x.p, "SELECT false"));
    // Another synchronous commit makes it durable: confirmed at once.
    rows(&mut x.s, "DROP TABLE local_only");
    let start = Instant::now();
    let yes = rows(&mut x.p, "SELECT true");
    wait_for(&mut x.p, flushed, &yes);
    assert!(start.elapsed() < Duration::from_secs(5), "confirmed after {:?}", start.elapsed());
}

/// Walsenders of one database decode each transaction once between them (`decode::Shared`),
/// and each still sends exactly its own output: a row-filtered publication, a column list,
/// binary and text transfer, and test_decoding through the SQL functions, read at once.
#[test]
fn shared_decoding_keeps_each_output() {
    let mut x = pair();
    let ddl = "CREATE TABLE t (id int PRIMARY KEY, a int, b text)";
    x.both(ddl);
    rows(&mut x.s, "CREATE DATABASE d2");
    rows(&mut x.s, "CREATE DATABASE d3");
    let mut s2 = x._sub_engine.connect(2, "d2", "postgres", "t").unwrap();
    let mut s3 = x._sub_engine.connect(3, "d3", "postgres", "t").unwrap();
    rows(&mut s2, ddl);
    rows(&mut s3, ddl);
    rows(&mut x.p, "CREATE PUBLICATION p_all FOR TABLE t");
    rows(&mut x.p, "CREATE PUBLICATION p_even FOR TABLE t WHERE (id % 2 = 0)");
    rows(&mut x.p, "CREATE PUBLICATION p_cols FOR TABLE t (id, a)");
    rows(&mut x.p, "SELECT 'init' FROM pg_create_logical_replication_slot('td', 'test_decoding')");
    x.subscribe("s_all", "p_all", "copy_data = false, binary = true");
    let conn = format!("host=127.0.0.1 port={} user=postgres dbname=postgres", x.port);
    rows(&mut s2, &format!("CREATE SUBSCRIPTION s_even CONNECTION '{conn}' PUBLICATION p_even WITH (copy_data = false)"));
    rows(&mut s3, &format!("CREATE SUBSCRIPTION s_cols CONNECTION '{conn}' PUBLICATION p_cols WITH (copy_data = false)"));
    wait_for(&mut x.p, "SELECT count(*) FROM pg_stat_replication", &["[Int8(3)]".to_string()]);
    for i in 0..60 {
        rows(&mut x.p, &format!("INSERT INTO t SELECT g, g * 3, 'b' || g FROM generate_series({}, {}) g", i * 10, i * 10 + 9));
        if i % 5 == 4 {
            rows(&mut x.p, &format!("UPDATE t SET a = a + 1, b = b || 'u' WHERE id % 3 = {}", i % 3));
        }
        if i % 7 == 6 {
            rows(&mut x.p, &format!("DELETE FROM t WHERE id % 11 = {}", i % 11));
        }
    }
    x.same("SELECT * FROM t ORDER BY id");
    let even = rows(&mut x.p, "SELECT * FROM t WHERE id % 2 = 0 ORDER BY id");
    wait_for(&mut s2, "SELECT * FROM t ORDER BY id", &even);
    let cols = rows(&mut x.p, "SELECT id, a, true FROM t ORDER BY id");
    wait_for(&mut s3, "SELECT id, a, b IS NULL FROM t ORDER BY id", &cols);
    let td = rows(
        &mut x.p,
        "SELECT count(*) FILTER (WHERE data LIKE 'table public.t: INSERT:%'), \
         count(*) FILTER (WHERE data LIKE 'table public.t: DELETE:%') FROM pg_logical_slot_peek_changes('td', NULL, NULL)",
    );
    assert_eq!(td, rows(&mut x.p, "SELECT 600::bigint, 600 - count(*) FROM t"));
    rows(&mut s2, "DROP SUBSCRIPTION s_even");
    rows(&mut s3, "DROP SUBSCRIPTION s_cols");
}
