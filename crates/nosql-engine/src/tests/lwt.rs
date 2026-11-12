//! Lightweight transactions beyond the captured cases (`captured_lwt`): linearisability under
//! concurrency, ballot timestamps, the PREPARED and RESULT flags the server needs, and paths the
//! references can't be probed on safely.

use std::sync::Arc;

use super::*;
use crate::Consistency;

const SCHEMA: &str =
    "CREATE TABLE ks.t (p int, c int, s int static, v int, l list<int>, PRIMARY KEY (p, c))";

async fn setup(d: Dialect) -> H {
    let mut h = H::new(d).await;
    h.exec(&format!("{KS};\n{SCHEMA}")).await;
    h
}

/// Concurrent compare-and-set increments from many sessions never lose an update.
#[tokio::test]
async fn concurrent_cas_is_linearisable() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.ok("INSERT INTO ks.t (p, c, v) VALUES (1, 1, 0)").await;
        let db = h.db.clone();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let db: Arc<Database> = db.clone();
            tasks.push(tokio::spawn(async move {
                let mut s = db.connect(Default::default());
                let mut done = 0;
                while done < 10 {
                    let r = s
                        .query(
                            "SELECT v FROM ks.t WHERE p = 1 AND c = 1",
                            QueryParams {
                                consistency: Consistency::Serial,
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                    let v = fmt_rows(&r)[0][0].parse::<i32>().unwrap();
                    let r = s
                        .query(
                            &format!(
                                "UPDATE ks.t SET v = {} WHERE p = 1 AND c = 1 IF v = {v}",
                                v + 1
                            ),
                            QueryParams::default(),
                        )
                        .await
                        .unwrap();
                    if fmt_rows(&r)[0][0] == "True" {
                        done += 1;
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE p = 1 AND c = 1").await,
            table(&[&["80"]]),
            "{d}"
        );
    }
}

/// ScyllaDB's `v = v + 1` needs no client-side retry loop.
#[tokio::test]
async fn concurrent_scylla_increments() {
    let mut h = setup(Dialect::Scylla).await;
    h.ok("INSERT INTO ks.t (p, c, v) VALUES (1, 1, 0)").await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = h.db.clone();
        tasks.push(tokio::spawn(async move {
            let mut s = db.connect(Default::default());
            for _ in 0..10 {
                s.query(
                    "UPDATE ks.t SET v = v + 1 WHERE p = 1 AND c = 1 IF EXISTS",
                    QueryParams::default(),
                )
                .await
                .unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        h.rows("SELECT v FROM ks.t WHERE p = 1 AND c = 1").await,
        table(&[&["80"]])
    );
}

/// Conditional writes use the ballot's timestamp: never the client's default timestamp nor a
/// batch member's `USING TIMESTAMP`; Cassandra's ballots are milliseconds × 1000.
#[tokio::test]
async fn ballot_timestamps() {
    for d in BOTH {
        let mut h = setup(d).await;
        let p = QueryParams {
            timestamp: Some(1000),
            ..Default::default()
        };
        h.s.query("INSERT INTO ks.t (p, c, v) VALUES (1, 1, 1)", p.clone())
            .await
            .unwrap();
        h.s.query(
            "INSERT INTO ks.t (p, c, v) VALUES (1, 2, 1) IF NOT EXISTS",
            p.clone(),
        )
        .await
        .unwrap();
        h.s.query(
            "BEGIN BATCH UPDATE ks.t SET v = 2 WHERE p = 1 AND c = 2 IF v = 1; UPDATE ks.t USING TIMESTAMP 5 SET v = 2 WHERE p = 1 AND c = 3; APPLY BATCH",
            p,
        )
        .await
        .unwrap();
        let rows = h.rows("SELECT c, writetime(v) FROM ks.t WHERE p = 1").await;
        assert_eq!(rows[0], ["1", "1000"], "{d}");
        let ts: Vec<i64> = rows[1..].iter().map(|r| r[1].parse().unwrap()).collect();
        assert!(
            ts.iter().all(|&t| t > 1_600_000_000_000_000),
            "{d}: {rows:?}"
        );
        assert_eq!(ts[0], ts[1], "{d}: one ballot for the batch");
        if !d.is_scylla() {
            assert!(ts.iter().all(|t| t % 1000 < 10), "{d}: {rows:?}");
        }
        // A plain write right after a conditional one still wins.
        h.ok("UPDATE ks.t SET v = 3 WHERE p = 1 AND c = 2 IF v = 2")
            .await;
        h.ok("UPDATE ks.t SET v = 4 WHERE p = 1 AND c = 2").await;
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE p = 1 AND c = 2").await,
            table(&[&["4"]]),
            "{d}"
        );
    }
}

/// PREPARED says a statement is conditional (ScyllaDB's LWT metadata mark), and conditional
/// results are flagged for the server's metadata rules.
#[tokio::test]
async fn lwt_flags() {
    for d in BOTH {
        let mut h = setup(d).await;
        for (cql, lwt) in [
            ("UPDATE ks.t SET v = ? WHERE p = ? AND c = ? IF v = ?", true),
            (
                "INSERT INTO ks.t (p, c, v) VALUES (?, ?, ?) IF NOT EXISTS",
                true,
            ),
            (
                "BEGIN BATCH UPDATE ks.t SET v = ? WHERE p = ? AND c = ? IF EXISTS; UPDATE ks.t SET v = 1 WHERE p = ? AND c = 2; APPLY BATCH",
                true,
            ),
            ("UPDATE ks.t SET v = ? WHERE p = ? AND c = ?", false),
            (
                "BEGIN BATCH UPDATE ks.t SET v = ? WHERE p = ? AND c = ?; APPLY BATCH",
                false,
            ),
            ("SELECT * FROM ks.t WHERE p = ?", false),
        ] {
            let p = h.s.prepare(cql, None).await.unwrap();
            assert_eq!(p.is_lwt, lwt, "{d}: {cql}");
            if lwt {
                assert_eq!(p.result.is_some(), d.is_scylla(), "{d}: {cql}");
            }
        }
        let r = h
            .ok("UPDATE ks.t SET v = 1 WHERE p = 1 AND c = 1 IF EXISTS")
            .await;
        assert!(r.conditional, "{d}");
        assert!(!h.ok("SELECT * FROM ks.t").await.conditional, "{d}");
    }
}

/// The hook is the only way to conditional execution: without it the core refuses.
#[tokio::test]
async fn without_the_hook() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.db.set_hooks(crate::hooks::Hooks::default());
        let e = h
            .err("UPDATE ks.t SET v = 1 WHERE p = 1 AND c = 1 IF v = 2")
            .await;
        assert_eq!(
            e.message, "Conditional updates are not supported yet",
            "{d}"
        );
    }
}

/// ScyllaDB drops the connection for a batch mixing `INSERT JSON … IF NOT EXISTS` with other
/// conditions (not probed again: it disturbs the reference); roll answers a server error.
#[tokio::test]
async fn scylla_json_mixed_batch() {
    let mut h = setup(Dialect::Scylla).await;
    let e = h
        .err("BEGIN BATCH INSERT INTO ks.t JSON '{\"p\": 1, \"c\": 1}' IF NOT EXISTS; UPDATE ks.t SET v = 7 WHERE p = 1 AND c = 1 IF v = 5; APPLY BATCH")
        .await;
    assert_eq!(e.code, ErrorCode::Server);
}

/// List operations by index and `c = c ± v` read the state under the partition lock, after the
/// conditions held (their errors only come when the conditions hold).
#[tokio::test]
async fn read_before_write_after_conditions() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.ok("INSERT INTO ks.t (p, c, v, l) VALUES (1, 1, 1, [1, 2])")
            .await;
        h.ok("UPDATE ks.t SET l[1] = 5 WHERE p = 1 AND c = 1 IF v = 1")
            .await;
        assert_eq!(
            h.rows("SELECT l FROM ks.t WHERE p = 1 AND c = 1").await,
            table(&[&["[1, 5]"]]),
            "{d}"
        );
        // Not applied: the bad index is never looked at.
        h.ok("UPDATE ks.t SET l[9] = 5 WHERE p = 1 AND c = 1 IF v = 2")
            .await;
        assert_eq!(
            h.msg("UPDATE ks.t SET l[9] = 5 WHERE p = 1 AND c = 1 IF v = 1")
                .await,
            "List index 9 out of bound, list has size 2",
            "{d}"
        );
        // Cassandra sees the statement's own pending write.
        h.ok("UPDATE ks.t SET v = v + 1, v = v + 1 WHERE p = 1 AND c = 1 IF EXISTS")
            .await;
        let want = if d.is_scylla() { "2" } else { "3" };
        assert_eq!(
            h.rows("SELECT v FROM ks.t WHERE p = 1 AND c = 1").await,
            table(&[&[want]]),
            "{d}"
        );
    }
}
