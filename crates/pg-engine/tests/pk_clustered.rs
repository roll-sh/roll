//! `roll_pk_clustered`: roll's own storage parameter (not Postgres's). A table with it on
//! stores its rows keyed by its (non-deferrable) primary key instead of under a hidden row
//! id in heap order, so scans come out in key order; everything else behaves as in the
//! default layout. ALTER TABLE SET / RESET moves a table between the layouts with its
//! data, indexes, constraints, heap TIDs and xmins kept.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::Session;
use std::time::Duration;

fn open(dir: &std::path::Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
    })
    .unwrap()
}

/// The last result's rows, or the error (with its detail).
fn out(s: &mut Session, sql: &str) -> Vec<String> {
    match s.query(sql) {
        Ok(r) => r
            .last()
            .map(|q| q.rows.iter().map(|row| format!("{row:?}")).collect())
            .unwrap_or_default(),
        Err(e) => vec![format!("ERROR: {} {:?}", e.message, e.detail)],
    }
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    let r = s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.last()
        .unwrap()
        .rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

/// Each statement on its own (VACUUM can't run in a multi-statement query).
fn run(s: &mut Session, sql: &str) {
    for stmt in sql.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        s.query(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

fn err(s: &mut Session, sql: &str) -> String {
    match s.query(sql) {
        Ok(_) => panic!("{sql}: no error"),
        Err(e) => {
            s.query("ROLLBACK").ok();
            format!("{} {}", e.message, e.detail.unwrap_or_default())
        }
    }
}

/// One-integer rows (`[Int4(3)]`) as integers.
fn ints(v: &[String]) -> Vec<i64> {
    v.iter()
        .map(|r| {
            let inner = r.split_once('(').and_then(|(_, x)| x.split_once(')')).unwrap().0;
            inner.parse().unwrap()
        })
        .collect()
}

fn sorted(v: &[i64]) -> Vec<i64> {
    let mut s = v.to_vec();
    s.sort();
    s
}

/// The same statements on a table of each layout (`{o}` is the WITH clause's option): the
/// same results, row for row (statements separated by `;\n`).
const PARITY: &str = r#"
CREATE TABLE p (id int PRIMARY KEY, v text, n int) WITH ({o});
CREATE INDEX p_v ON p (v);
CREATE UNIQUE INDEX p_n ON p (n);
INSERT INTO p VALUES (5, 'e', 50), (1, 'a', 10), (3, 'c', 30) RETURNING *;
INSERT INTO p VALUES (1, 'dup', 99);
INSERT INTO p VALUES (9, 'dup', 10);
INSERT INTO p VALUES (1, 'x', 11) ON CONFLICT DO NOTHING RETURNING *;
INSERT INTO p VALUES (1, 'x', 11) ON CONFLICT (id) DO UPDATE SET v = excluded.v || '!' RETURNING *;
INSERT INTO p VALUES (7, 'g', 70), (3, 'cc', 31) ON CONFLICT (id) DO UPDATE SET v = excluded.v RETURNING *;
INSERT INTO p VALUES (8, 'h', 10) ON CONFLICT (n) DO UPDATE SET v = 'via n' RETURNING *;
INSERT INTO p VALUES (8, 'h', 80) ON CONFLICT ON CONSTRAINT p_pkey DO NOTHING RETURNING *;
INSERT INTO p SELECT g, 'g' || g, g * 10 FROM generate_series(20, 11, -1) g;
UPDATE p SET n = n + 1 WHERE id = 5 RETURNING *;
UPDATE p SET id = id + 100 WHERE id IN (1, 3) RETURNING *;
UPDATE p SET id = 7 WHERE id = 5;
UPDATE p SET n = 700 WHERE id = 7 RETURNING id, n;
DELETE FROM p WHERE id = 7 RETURNING *;
SELECT * FROM p ORDER BY id;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT * FROM p WHERE id = 103;
SELECT id, v FROM p WHERE id BETWEEN 2 AND 15 ORDER BY id;
SELECT id FROM p WHERE v = 'x!';
SELECT id FROM p WHERE n > 120 ORDER BY n DESC;
SELECT id FROM p ORDER BY id DESC LIMIT 3;
RESET enable_seqscan;
RESET enable_bitmapscan;
SELECT count(*), sum(id), min(v), max(n) FROM p;
CREATE TABLE c (id int PRIMARY KEY, pid int REFERENCES p ON DELETE CASCADE ON UPDATE CASCADE, w text) WITH ({o});
INSERT INTO c VALUES (1, 103, 'a'), (2, 5, 'b'), (3, 12, 'c');
INSERT INTO c VALUES (4, 999, 'z');
UPDATE p SET id = 300 WHERE id = 103;
SELECT * FROM c ORDER BY id;
DELETE FROM p WHERE id = 5;
SELECT * FROM c ORDER BY id;
CREATE TABLE r (id int PRIMARY KEY, pid int REFERENCES p, w text) WITH ({o});
INSERT INTO r VALUES (1, 300, 'r');
DELETE FROM p WHERE id = 300;
UPDATE p SET id = 301 WHERE id = 300;
CREATE TABLE ref2 (a int, b text, PRIMARY KEY (a, b)) WITH ({o});
INSERT INTO ref2 VALUES (1, 'x'), (1, 'a'), (0, 'z');
CREATE TABLE ch2 (a int, b text, FOREIGN KEY (a, b) REFERENCES ref2) WITH ({o});
INSERT INTO ch2 VALUES (1, 'a'), (NULL, 'q');
INSERT INTO ch2 VALUES (1, 'b');
SELECT * FROM ref2 ORDER BY a, b;
CREATE TABLE log (op text, old text, new text);
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES (TG_OP || ' ' || TG_WHEN, OLD::text, NEW::text); IF TG_WHEN = 'BEFORE' AND TG_OP = 'INSERT' THEN NEW.v := NEW.v || '+'; END IF; RETURN COALESCE(NEW, OLD); END $$;
CREATE TRIGGER tb BEFORE INSERT OR UPDATE OR DELETE ON p FOR EACH ROW EXECUTE FUNCTION trg();
CREATE TRIGGER ta AFTER INSERT OR UPDATE OR DELETE ON p FOR EACH ROW EXECUTE FUNCTION trg();
INSERT INTO p VALUES (40, 'forty', 400) RETURNING *;
UPDATE p SET id = 41 WHERE id = 40 RETURNING *;
DELETE FROM p WHERE id = 41 RETURNING *;
INSERT INTO p VALUES (12, 'conflict', 1) ON CONFLICT (id) DO UPDATE SET v = 'upd ' || excluded.v RETURNING *;
SELECT * FROM log;
DROP TRIGGER tb ON p;
DROP TRIGGER ta ON p;
BEGIN;
DECLARE cur CURSOR FOR SELECT id, v FROM p WHERE id < 200 ORDER BY id FOR UPDATE;
FETCH 1 FROM cur;
UPDATE p SET v = 'cursor' WHERE CURRENT OF cur RETURNING *;
FETCH 1 FROM cur;
DELETE FROM p WHERE CURRENT OF cur RETURNING *;
FETCH 1 FROM cur;
UPDATE p SET id = 1000 WHERE CURRENT OF cur RETURNING *;
COMMIT;
SELECT * FROM p ORDER BY id;
SELECT count(*) FROM p WHERE ctid = ANY (ARRAY(SELECT ctid FROM p));
SELECT a.id FROM p a JOIN p b ON a.ctid = b.ctid ORDER BY 1;
SELECT id FROM p WHERE ctid = (SELECT ctid FROM p WHERE id = 15);
SELECT id FROM p WHERE ctid = (SELECT ctid FROM p WHERE id = 15) FOR UPDATE;
VACUUM p;
VACUUM (FULL) p;
SELECT * FROM p ORDER BY id;
CLUSTER p USING p_v;
SELECT * FROM p ORDER BY id;
VACUUM (ANALYZE) p;
SELECT pg_relation_size('p') > 0, pg_total_relation_size('p') > 0, pg_relation_size('p_pkey') > 0, pg_indexes_size('p') > 0;
SELECT relpages > 0, reltuples FROM pg_class WHERE relname = 'p';
SET enable_seqscan = off;
SELECT v FROM p WHERE id = 12;
SELECT id FROM p WHERE v = 'cursor';
RESET enable_seqscan;
TRUNCATE r;
TRUNCATE p CASCADE;
SELECT (SELECT count(*) FROM p), (SELECT count(*) FROM c);
INSERT INTO p VALUES (2, 'b', 2), (1, 'a', 1) RETURNING *;
SELECT * FROM p ORDER BY id;
CREATE TABLE pt (k int, v text, PRIMARY KEY (k)) PARTITION BY RANGE (k) WITH ({o});
CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (10) TO (100);
INSERT INTO pt VALUES (15, 'a'), (5, 'b'), (1, 'c'), (12, 'd');
INSERT INTO pt VALUES (5, 'dup');
INSERT INTO pt VALUES (5, 'x') ON CONFLICT (k) DO UPDATE SET v = 'upd' RETURNING *;
UPDATE pt SET k = 50 WHERE k = 1 RETURNING tableoid::regclass::text, *;
UPDATE pt SET k = 15 WHERE k = 12;
DELETE FROM pt WHERE k = 15 RETURNING *;
SELECT tableoid::regclass::text, * FROM pt ORDER BY k;
CREATE TABLE pt3 (k int NOT NULL, v text) WITH ({o});
INSERT INTO pt3 VALUES (150, 'z'), (120, 'y');
ALTER TABLE pt ATTACH PARTITION pt3 FOR VALUES FROM (100) TO (200);
INSERT INTO pt VALUES (120, 'again');
SELECT tableoid::regclass::text, * FROM pt ORDER BY k;
ALTER TABLE pt DETACH PARTITION pt3;
SELECT * FROM pt3 ORDER BY k;
CREATE TABLE ip (id int PRIMARY KEY, v text) WITH ({o});
CREATE TABLE ic (extra int) INHERITS (ip) WITH ({o});
ALTER TABLE ic ADD PRIMARY KEY (id);
INSERT INTO ip VALUES (2, 'p2'), (1, 'p1');
INSERT INTO ic VALUES (4, 'c4', 1), (3, 'c3', 2);
INSERT INTO ic VALUES (3, 'dup', 0);
SELECT tableoid::regclass::text, * FROM ip ORDER BY id;
UPDATE ip SET v = v || '!' WHERE id IN (1, 3);
DELETE FROM ip WHERE id = 4;
SELECT * FROM ic ORDER BY id;
SELECT tableoid::regclass::text, * FROM ip ORDER BY id;
CREATE TABLE gen (a int, b int GENERATED ALWAYS AS (a * 2) STORED, c text DEFAULT 'd', PRIMARY KEY (b)) WITH ({o});
INSERT INTO gen (a) VALUES (3), (1), (2) RETURNING *;
UPDATE gen SET a = 10 WHERE a = 1 RETURNING *;
INSERT INTO gen (a) VALUES (10);
COPY gen (a, c) FROM STDIN;
SELECT * FROM gen ORDER BY b;
CREATE TABLE tx (id int PRIMARY KEY, v int) WITH ({o});
BEGIN;
INSERT INTO tx VALUES (1, 1), (2, 2);
SAVEPOINT s;
UPDATE tx SET id = 3 WHERE id = 1;
INSERT INTO tx VALUES (1, 10);
ROLLBACK TO s;
UPDATE tx SET v = v + 100;
COMMIT;
SELECT * FROM tx ORDER BY id;
BEGIN;
DELETE FROM tx WHERE id = 1;
INSERT INTO tx VALUES (1, 1000);
SELECT * FROM tx ORDER BY id;
ROLLBACK;
SELECT * FROM tx ORDER BY id;
MERGE INTO tx USING (VALUES (2, 5), (7, 7)) AS s (id, v) ON tx.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v) RETURNING merge_action(), tx.*;
SELECT * FROM tx ORDER BY id;
SELECT * FROM tx WHERE id IN (7, 2) ORDER BY id FOR SHARE;
ALTER TABLE tx ADD COLUMN w text DEFAULT 'w';
ALTER TABLE tx ALTER COLUMN v TYPE bigint;
ALTER TABLE tx ALTER COLUMN id TYPE bigint;
INSERT INTO tx VALUES (5, 5, 'five');
INSERT INTO tx VALUES (5, 6, 'dup');
ALTER TABLE tx DROP COLUMN w;
REINDEX TABLE tx;
REINDEX INDEX tx_pkey;
SET enable_seqscan = off;
SELECT * FROM tx WHERE id = 5;
RESET enable_seqscan;
SELECT * FROM tx ORDER BY id;
ALTER TABLE tx SET UNLOGGED;
ALTER TABLE tx SET LOGGED;
UPDATE tx SET v = -v WHERE id > 2 RETURNING *;
CREATE TABLE lk (LIKE tx INCLUDING ALL) WITH ({o});
INSERT INTO lk SELECT * FROM tx;
INSERT INTO lk VALUES (1, 0);
SELECT * FROM lk ORDER BY id;
CREATE TABLE ma (id int NOT NULL, v text) WITH ({o});
INSERT INTO ma VALUES (2, 'b'), (1, 'a');
ALTER TABLE ma ADD PRIMARY KEY (id), ADD COLUMN w int DEFAULT 0;
INSERT INTO ma VALUES (1, 'dup');
SELECT * FROM ma ORDER BY id;
CREATE TEMP TABLE tt (id int PRIMARY KEY, v text) WITH ({o});
INSERT INTO tt VALUES (2, 'b'), (1, 'a') ON CONFLICT DO NOTHING;
UPDATE tt SET id = 3 WHERE id = 1;
SELECT * FROM tt ORDER BY id;
"#;

/// Run `PARITY` with `{o}` replaced, one result per statement.
fn parity_run(s: &mut Session, opt: &str) -> Vec<(String, Vec<String>)> {
    let mut res = Vec::new();
    for stmt in PARITY.split(";\n").map(str::trim).filter(|q| !q.is_empty()) {
        let stmt = stmt.replace("{o}", opt);
        let r = if stmt.starts_with("COPY") {
            match s.query_copy(&stmt, b"5\tcopied\n4\tcopied too\n") {
                Ok(_) => vec!["copied".to_string()],
                Err(e) => vec![format!("ERROR: {}", e.message)],
            }
        } else {
            out(s, &stmt)
        };
        res.push((stmt, r));
    }
    res
}

#[test]
fn layouts_agree() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    s.query("CREATE DATABASE dflt").unwrap();
    s.query("CREATE DATABASE clus").unwrap();
    let mut d = engine.connect(2, "dflt", "postgres", "t").unwrap();
    let mut c = engine.connect(3, "clus", "postgres", "t").unwrap();
    let a = parity_run(&mut d, "roll_pk_clustered = false");
    let b = parity_run(&mut c, "roll_pk_clustered = true");
    for ((stmt, x), (_, y)) in a.iter().zip(&b) {
        if std::env::var_os("PK_PARITY_PRINT").is_some() {
            eprintln!("{stmt}\n  {x:?}");
        }
        assert_eq!(x, y, "{stmt}");
        assert!(
            !x.iter().any(|r| r.starts_with("ERROR") && r.contains("internal")),
            "{stmt}: {x:?}"
        );
    }
    // (The script does what it means to: spot checks.)
    let find = |stmt: &str| &a.iter().find(|(s, _)| s.starts_with(stmt)).unwrap().1;
    assert!(find("INSERT INTO p VALUES (1, 'dup'")[0].contains("p_pkey"));
    assert!(find("INSERT INTO c VALUES (4, 999")[0].contains("c_pid_fkey"));
    assert!(find("DELETE FROM p WHERE id = 300")[0].contains("r_pid_fkey"));
    assert!(find("INSERT INTO pt VALUES (5, 'dup')")[0].contains("pt1_pkey"));
    assert!(find("UPDATE p SET v = 'cursor' WHERE CURRENT OF cur")[0].contains("cursor"));
    assert_eq!(find("SELECT id FROM p WHERE ctid = (SELECT ctid FROM p WHERE id = 15)"), &["[Int4(15)]"]);

    // The partitions took the partitioned table's option; the clustered tables scan in key
    // order, the default ones in heap order.
    assert_eq!(
        rows(&mut c, "SELECT relname, array_to_string(reloptions, ',') FROM pg_class WHERE relname IN ('p', 'pt', 'pt1', 'pt2') ORDER BY 1"),
        [
            "[Text(\"p\"), Text(\"roll_pk_clustered=true\")]",
            "[Text(\"pt\"), Text(\"roll_pk_clustered=true\")]",
            "[Text(\"pt1\"), Text(\"roll_pk_clustered=true\")]",
            "[Text(\"pt2\"), Text(\"roll_pk_clustered=true\")]",
        ]
    );
    for s in [&mut c, &mut d] {
        run(s, "CREATE TABLE ord (id int PRIMARY KEY, v text);\nINSERT INTO ord SELECT g, 'v' FROM generate_series(10, 1, -1) g");
    }
    run(&mut c, "ALTER TABLE ord SET (roll_pk_clustered = true)");
    let ck = ints(&rows(&mut c, "SELECT id FROM ord"));
    assert_eq!(ck, (1..=10).collect::<Vec<_>>());
    let dk = ints(&rows(&mut d, "SELECT id FROM ord"));
    assert_eq!(dk, (1..=10).rev().collect::<Vec<_>>());
    let ck = ints(&rows(&mut c, "SELECT k FROM pt"));
    assert_eq!(ck, sorted(&ck));
    engine.shutdown();
}

#[test]
fn option_rules() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    // Needs a non-deferrable primary key.
    let e = err(&mut s, "CREATE TABLE e1 (id int PRIMARY KEY DEFERRABLE) WITH (roll_pk_clustered = true)");
    assert!(e.contains("requires a non-deferrable primary key") && e.contains("deferrable"), "{e}");
    run(&mut s, "CREATE TABLE e2 (id int)");
    let e = err(&mut s, "ALTER TABLE e2 SET (roll_pk_clustered = true)");
    assert!(e.contains("requires a non-deferrable primary key") && e.contains("has no primary key"), "{e}");
    run(&mut s, "CREATE TABLE e4 (id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)");
    let e = err(&mut s, "ALTER TABLE e4 SET (roll_pk_clustered = on)");
    assert!(e.contains("deferrable primary key \"e4_pkey\""), "{e}");
    // A table made without its key (as pg_dump makes them) waits for one.
    run(&mut s, "CREATE TABLE e3 (id int NOT NULL, v text) WITH (roll_pk_clustered = true);\nINSERT INTO e3 VALUES (3, 'c'), (1, 'a'), (2, 'b')");
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM e3")), [3, 1, 2]);
    let e = err(&mut s, "ALTER TABLE e3 ADD PRIMARY KEY (id) DEFERRABLE");
    assert!(e.contains("requires a non-deferrable primary key"), "{e}");
    run(&mut s, "INSERT INTO e3 VALUES (1, 'dup')");
    let e = err(&mut s, "ALTER TABLE e3 ADD PRIMARY KEY (id)");
    assert!(e.contains("could not create unique index \"e3_pkey\"") && e.contains("Key (id)=(1) is duplicated."), "{e}");
    run(&mut s, "DELETE FROM e3 WHERE v = 'dup';\nALTER TABLE e3 ADD PRIMARY KEY (id)");
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM e3")), [1, 2, 3]);
    // Nor with a primary key WITHOUT OVERLAPS.
    let e = err(&mut s, "CREATE TABLE e6 (id int4range, during int4range, PRIMARY KEY (id, during WITHOUT OVERLAPS)) WITH (roll_pk_clustered = true)");
    assert!(e.contains("WITHOUT OVERLAPS"), "{e}");
    run(&mut s, "CREATE TABLE e6 (id int4range, during int4range, PRIMARY KEY (id, during WITHOUT OVERLAPS))");
    let e = err(&mut s, "ALTER TABLE e6 SET (roll_pk_clustered = true)");
    assert!(e.contains("WITHOUT OVERLAPS"), "{e}");
    // A key added in the same ALTER TABLE.
    run(&mut s, "CREATE TABLE e7 (id int NOT NULL, v text);\nINSERT INTO e7 VALUES (2, 'b'), (1, 'a');\nALTER TABLE e7 ADD PRIMARY KEY (id), SET (roll_pk_clustered = true)");
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM e7")), [1, 2]);
    // Postgres's own errors stay as they were.
    assert_eq!(err(&mut s, "ALTER TABLE e3 SET (bogus = 1)").trim(), "unrecognized parameter \"bogus\"");
    assert_eq!(
        err(&mut s, "ALTER TABLE e3 SET (roll_pk_clustered = maybe)").trim(),
        "invalid value for boolean option \"roll_pk_clustered\": maybe"
    );
    assert!(err(&mut s, "CREATE TABLE pp (k int) PARTITION BY LIST (k) WITH (fillfactor = 50)")
        .contains("cannot specify storage parameters for a partitioned table"));
    assert!(err(&mut s, "CREATE TABLE pp (k int PRIMARY KEY) PARTITION BY LIST (k) WITH (roll_pk_clustered = true, fillfactor = 50)")
        .contains("cannot specify storage parameters for a partitioned table"));
    assert!(err(&mut s, "CREATE MATERIALIZED VIEW mv WITH (roll_pk_clustered = true) AS SELECT 1 AS a")
        .contains("is a materialized view"));
    run(&mut s, "CREATE MATERIALIZED VIEW mv AS SELECT 1 AS a");
    assert!(err(&mut s, "ALTER MATERIALIZED VIEW mv SET (roll_pk_clustered = true)").contains("is a materialized view"));
    // reloptions round-trip: \d+ shows it, a dump's CREATE TABLE takes it back.
    run(&mut s, "ALTER TABLE e3 SET (fillfactor = 90)");
    let opts = rows(&mut s, "SELECT array_to_string(reloptions, ', ') FROM pg_class WHERE relname = 'e3'");
    assert_eq!(opts, ["[Text(\"roll_pk_clustered=true, fillfactor=90\")]"]);
    run(&mut s, "CREATE TABLE e5 (id int NOT NULL, v text) WITH (roll_pk_clustered='true', fillfactor='90');\nINSERT INTO e5 VALUES (2, 'b'), (1, 'a');\nALTER TABLE ONLY e5 ADD CONSTRAINT e5_pkey PRIMARY KEY (id)");
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM e5")), [1, 2]);
    // RESET leaves the others.
    run(&mut s, "ALTER TABLE e3 RESET (roll_pk_clustered)");
    assert_eq!(
        rows(&mut s, "SELECT array_to_string(reloptions, ', ') FROM pg_class WHERE relname = 'e3'"),
        ["[Text(\"fillfactor=90\")]"]
    );
    engine.shutdown();
}

/// A table written to so that its heap order isn't its key order, with indexes of several
/// kinds and a table referencing it.
const SWITCH: &str = "
CREATE TABLE sw (id int PRIMARY KEY, v text, w int);
CREATE INDEX sw_v ON sw (v);
CREATE UNIQUE INDEX sw_w ON sw (w DESC NULLS LAST);
CREATE INDEX sw_expr ON sw ((w % 7)) WHERE id % 2 = 0;
INSERT INTO sw SELECT g, 'v' || (g % 9), g * 3 FROM generate_series(300, 1, -1) g;
UPDATE sw SET v = 'upd' WHERE id % 10 = 0;
UPDATE sw SET id = id + 1000 WHERE id % 37 = 0;
DELETE FROM sw WHERE id % 13 = 0;
VACUUM sw;
INSERT INTO sw VALUES (0, 'zero', NULL), (5000, 'last', 15000);
CREATE TABLE swc (a int REFERENCES sw ON DELETE CASCADE, b text);
INSERT INTO swc VALUES (1, 'x'), (1037, 'y'), (20, 'z')
";

const SNAP: &str = "SELECT ctid, xmin, * FROM sw ORDER BY id";
const SIZES: &str = "SELECT pg_relation_size('sw'), pg_relation_size('sw_pkey'), pg_relation_size('sw_v'), pg_relation_size('sw_w')";

/// Index scans find what a sequential scan does; the constraints hold.
fn check_sw(s: &mut Session) {
    const QS: [&str; 5] = [
        "SELECT v FROM sw WHERE id = 1037",
        "SELECT id FROM sw WHERE id BETWEEN 50 AND 60 ORDER BY id",
        "SELECT id FROM sw WHERE v = 'upd' ORDER BY id",
        "SELECT id FROM sw WHERE w = 300",
        "SELECT id FROM sw WHERE w % 7 = 3 AND id % 2 = 0 ORDER BY id",
    ];
    let seq: Vec<Vec<String>> = QS.iter().map(|q| rows(s, q)).collect();
    s.query("SET enable_seqscan = off; SET enable_bitmapscan = off").unwrap();
    for (q, want) in QS.iter().zip(&seq) {
        assert!(!want.is_empty(), "{q}");
        assert_eq!(&rows(s, q), want, "{q}");
    }
    assert_eq!(rows(s, "SELECT id FROM sw WHERE w = 300"), ["[Int4(100)]"]);
    s.query("RESET enable_seqscan; RESET enable_bitmapscan").unwrap();
    assert!(err(s, "INSERT INTO sw VALUES (1037, 'dup', 1)").contains("sw_pkey"));
    assert!(err(s, "INSERT INTO sw VALUES (9999, 'dup', 300)").contains("sw_w"));
    assert!(err(s, "INSERT INTO swc VALUES (13, 'gone')").contains("swc_a_fkey"));
    assert!(err(s, "UPDATE sw SET id = 2 WHERE id = 3").contains("sw_pkey"));
}

#[test]
fn switching_layouts_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
    run(&mut s, SWITCH);
    let snap = rows(&mut s, SNAP);
    let sizes = rows(&mut s, SIZES);
    let heap_order = rows(&mut s, "SELECT id FROM sw");
    assert_ne!(ints(&heap_order), sorted(&ints(&heap_order)));
    check_sw(&mut s);

    // Clustered: the same rows (ctid and xmin too), in key order.
    run(&mut s, "ALTER TABLE sw SET (roll_pk_clustered = true)");
    assert_eq!(rows(&mut s, SNAP), snap);
    // (Nothing moved in the files.)
    assert_eq!(rows(&mut s, SIZES), sizes);
    let ids = ints(&rows(&mut s, "SELECT id FROM sw"));
    assert_eq!(ids, sorted(&ints(&heap_order)));
    check_sw(&mut s);
    // In a transaction that rolls back, nothing changes.
    s.query("BEGIN").unwrap();
    s.query("ALTER TABLE sw RESET (roll_pk_clustered)").unwrap();
    assert_eq!(rows(&mut s, "SELECT id FROM sw"), heap_order);
    s.query("ROLLBACK").unwrap();
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM sw")), ids);
    assert_eq!(rows(&mut s, SNAP), snap);

    // The writes since are in both layouts' terms.
    run(&mut s, "UPDATE sw SET v = 'moved' WHERE id = 4;\nDELETE FROM sw WHERE id = 2;\nUPDATE sw SET id = 2 WHERE id = 4;\nUPDATE sw SET v = 'v2' WHERE id = 2");
    let snap = rows(&mut s, SNAP);
    let after = rows(&mut s, "SELECT id FROM sw");

    // Back to the default: heap order again, the same rows.
    run(&mut s, "ALTER TABLE sw RESET (roll_pk_clustered)");
    assert_eq!(rows(&mut s, SNAP), snap);
    let back = ints(&rows(&mut s, "SELECT id FROM sw"));
    assert_ne!(back, ints(&after));
    assert_eq!(sorted(&back), ints(&after));
    check_sw(&mut s);
    run(&mut s, "INSERT INTO sw VALUES (-5, 'new', -15)");
    assert_eq!(rows(&mut s, "SELECT v FROM sw WHERE id = -5"), ["[Text(\"new\")]"]);

    // On again, then its key dropped: the default layout, and no option.
    run(&mut s, "ALTER TABLE sw SET (roll_pk_clustered = true, fillfactor = 80)");
    let snap = rows(&mut s, SNAP);
    run(&mut s, "ALTER TABLE swc DROP CONSTRAINT swc_a_fkey;\nALTER TABLE sw DROP CONSTRAINT sw_pkey");
    assert_eq!(
        rows(&mut s, "SELECT array_to_string(reloptions, ',') FROM pg_class WHERE relname = 'sw'"),
        ["[Text(\"fillfactor=80\")]"]
    );
    assert_eq!(rows(&mut s, SNAP), snap);
    let ids = ints(&rows(&mut s, "SELECT id FROM sw"));
    assert_ne!(ids, sorted(&ids));
    run(&mut s, "INSERT INTO sw VALUES (2, 'now a duplicate', 1)");
    assert_eq!(rows(&mut s, "SELECT count(*) FROM sw WHERE id = 2"), ["[Int8(2)]"]);
    // Dropping a column of a clustered key ends it too.
    run(&mut s, "CREATE TABLE dc (a int, b int, PRIMARY KEY (a)) WITH (roll_pk_clustered = true);\nINSERT INTO dc VALUES (2, 20), (1, 10);\nALTER TABLE dc DROP COLUMN a");
    assert_eq!(rows(&mut s, "SELECT reloptions FROM pg_class WHERE relname = 'dc'"), ["[Null]"]);
    assert_eq!(rows(&mut s, "SELECT b FROM dc ORDER BY b"), ["[Int4(10)]", "[Int4(20)]"]);

    // A partitioned table passes it to its partitions (each laid out again).
    run(&mut s, "CREATE TABLE pt (k int, v text, PRIMARY KEY (k)) PARTITION BY RANGE (k);\nCREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100);\nCREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (100) TO (200) PARTITION BY RANGE (k);\nCREATE TABLE pt21 PARTITION OF pt2 FOR VALUES FROM (100) TO (200);\nINSERT INTO pt SELECT g, 'p' FROM generate_series(199, 1, -2) g");
    let before = rows(&mut s, "SELECT tableoid::regclass, ctid, * FROM pt ORDER BY k");
    run(&mut s, "ALTER TABLE pt SET (roll_pk_clustered = true)");
    assert_eq!(rows(&mut s, "SELECT tableoid::regclass, ctid, * FROM pt ORDER BY k"), before);
    assert_eq!(
        rows(&mut s, "SELECT relname, array_to_string(reloptions, ',') FROM pg_class WHERE relname LIKE 'pt%' AND relkind IN ('r', 'p') ORDER BY 1"),
        [
            "[Text(\"pt\"), Text(\"roll_pk_clustered=true\")]",
            "[Text(\"pt1\"), Text(\"roll_pk_clustered=true\")]",
            "[Text(\"pt2\"), Text(\"roll_pk_clustered=true\")]",
            "[Text(\"pt21\"), Text(\"roll_pk_clustered=true\")]",
        ]
    );
    let ks = ints(&rows(&mut s, "SELECT k FROM pt1"));
    assert_eq!(ks, sorted(&ks));
    run(&mut s, "CREATE TABLE pt3 PARTITION OF pt FOR VALUES FROM (200) TO (300);\nINSERT INTO pt VALUES (250, 'a'), (210, 'b')");
    assert_eq!(ints(&rows(&mut s, "SELECT k FROM pt3")), [210, 250]);
    assert!(err(&mut s, "INSERT INTO pt VALUES (210, 'dup')").contains("pt3_pkey"));
    run(&mut s, "ALTER TABLE ONLY pt RESET (roll_pk_clustered)");
    assert_eq!(
        rows(&mut s, "SELECT array_to_string(reloptions, ',') FROM pg_class WHERE relname = 'pt1'"),
        ["[Text(\"roll_pk_clustered=true\")]"]
    );
    assert_eq!(rows(&mut s, "SELECT reloptions IS NULL FROM pg_class WHERE relname = 'pt'"), ["[Bool(true)]"]);
    run(&mut s, "ALTER TABLE pt SET (roll_pk_clustered = false)");
    assert_eq!(
        rows(&mut s, "SELECT relname, array_to_string(reloptions, ',') FROM pg_class WHERE relname IN ('pt', 'pt1', 'pt21', 'pt3') ORDER BY 1"),
        [
            "[Text(\"pt\"), Text(\"roll_pk_clustered=false\")]",
            "[Text(\"pt1\"), Null]",
            "[Text(\"pt21\"), Null]",
            "[Text(\"pt3\"), Null]",
        ]
    );
    assert_eq!(rows(&mut s, "SELECT tableoid::regclass, ctid, * FROM pt WHERE k < 200 ORDER BY k"), before);
    engine.shutdown();
}

#[test]
fn storage_upgrade_leaves_clustered_tables() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(dir.path());
    {
        let mut s = engine.connect(1, "postgres", "postgres", "t").unwrap();
        run(&mut s, "CREATE TABLE cl (id int PRIMARY KEY, v text) WITH (roll_pk_clustered = true);\nCREATE INDEX cl_v ON cl (v);\nINSERT INTO cl SELECT g, 'v' || g FROM generate_series(50, 1, -1) g;\nCREATE TABLE df (id int PRIMARY KEY, v text);\nINSERT INTO df SELECT g, 'v' FROM generate_series(5, 1, -1) g");
        assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (1, 0));
    }
    engine.downgrade_storage_for_test("postgres").unwrap();
    engine.shutdown();
    drop(engine);
    // Opened again: the upgrade runs, and moves nothing of the clustered table.
    let engine = open(dir.path());
    let mut s = engine.connect(2, "postgres", "postgres", "t").unwrap();
    assert_eq!(engine.storage_format_for_test("postgres").unwrap(), (1, 0));
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM cl")), (1..=50).collect::<Vec<_>>());
    assert_eq!(ints(&rows(&mut s, "SELECT id FROM df")), [5, 4, 3, 2, 1]);
    s.query("SET enable_seqscan = off; SET enable_bitmapscan = off").unwrap();
    assert_eq!(rows(&mut s, "SELECT id FROM cl WHERE v = 'v17'"), ["[Int4(17)]"]);
    assert_eq!(rows(&mut s, "SELECT v FROM cl WHERE id = 17"), ["[Text(\"v17\")]"]);
    assert!(err(&mut s, "INSERT INTO cl VALUES (17, 'dup')").contains("cl_pkey"));
    assert_eq!(
        rows(&mut s, "SELECT array_to_string(reloptions, ',') FROM pg_class WHERE relname = 'cl'"),
        ["[Text(\"roll_pk_clustered=true\")]"]
    );
    engine.shutdown();
}
