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
    Engine::open(EngineOptions {
        data_dir: data.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
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
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let publisher = open(d1.path(), &[], &[("wal_level", "logical"), ("logical_decoding_work_mem", "64kB")]);
    let port = pg_engine::server::spawn(publisher.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap().port();
    let subscriber = open(d2.path(), &["127.0.0.1"], &[]);
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
