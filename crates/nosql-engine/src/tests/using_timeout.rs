//! ScyllaDB's `USING TIMEOUT` (validation of literals and bound values, writes and TRUNCATE
//! that time out at once) and `restrict_future_timestamp`, probed on the reference.

use super::*;

fn duration(months: i32, days: i32, nanos: i64) -> Param {
    Param::new(Bytes::from(
        nosql_types::Duration::new(months, days, nanos).encode(),
    ))
}

#[tokio::test]
async fn timeout_values() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.u (p int PRIMARY KEY, v int)"
    ))
    .await;
    let neg = "Timeout values must be non-negative";
    let days = "Timeout values cannot be expressed in days/months";
    let fine = "Timeout values cannot have granularity finer than milliseconds";
    for (cql, want) in [
        ("SELECT * FROM ks.u USING TIMEOUT 0ms", None),
        ("SELECT * FROM ks.u USING TIMEOUT 1h", None),
        ("SELECT * FROM ks.u USING TIMEOUT 1ms AND TIMEOUT 2ms", None),
        ("SELECT * FROM ks.u USING TIMEOUT -1ms", Some(neg)),
        ("SELECT * FROM ks.u USING TIMEOUT 1mo", Some(days)),
        ("SELECT * FROM ks.u USING TIMEOUT 1d", Some(days)),
        ("SELECT * FROM ks.u USING TIMEOUT 1y", Some(days)),
        ("SELECT * FROM ks.u USING TIMEOUT 25d", Some(days)),
        ("SELECT * FROM ks.u USING TIMEOUT 1us", Some(fine)),
        ("SELECT * FROM ks.u USING TIMEOUT 1ns", Some(fine)),
        (
            "SELECT * FROM ks.u USING TIMEOUT 5",
            Some("Invalid INTEGER constant (5) for \"[timeout]\" of type duration"),
        ),
        (
            "SELECT * FROM ks.u USING TIMEOUT 'a'",
            Some("Invalid STRING constant (a) for \"[timeout]\" of type duration"),
        ),
        (
            "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMEOUT -5s",
            Some(neg),
        ),
        (
            "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMEOUT 1mo",
            Some(days),
        ),
        ("TRUNCATE ks.u USING TIMEOUT -1s", Some(neg)),
        ("TRUNCATE TABLE ks.u USING TIMEOUT 1mo", Some(days)),
        (
            "BEGIN BATCH USING TIMEOUT 1mo INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH",
            Some(days),
        ),
        ("TRUNCATE ks.u USING TIMEOUT 1s", None),
        (
            "BEGIN BATCH USING TIMEOUT 1s INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH",
            None,
        ),
        (
            "BEGIN BATCH INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMEOUT 1s; APPLY BATCH",
            None,
        ),
    ] {
        match (h.run(cql).await, want) {
            (Ok(_), None) => {}
            (Err(e), Some(w)) => assert_eq!(
                (e.code, e.message.as_str()),
                (ErrorCode::Invalid, w),
                "{cql}"
            ),
            (r, w) => panic!("{cql}: {:?}, want {w:?}", r.map(|r| r.body)),
        }
    }
    let e = h.err("TRUNCATE ks.u USING TIMESTAMP 1").await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::Syntax, "line 1:20 : Missing K_TIMEOUT")
    );
    // A zero timeout times writes out at once.
    for cql in [
        "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMEOUT 0ms",
        "UPDATE ks.u USING TIMEOUT 0s SET v = 1 WHERE p = 1",
        "DELETE FROM ks.u USING TIMEOUT 0s WHERE p = 1",
        "BEGIN BATCH USING TIMEOUT 0s INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH",
    ] {
        let e = h.err(cql).await;
        assert_eq!(
            (e.code, e.message.as_str()),
            (
                ErrorCode::WriteTimeout,
                "Operation timed out for ks.u - received only 0 responses from 1 CL=ONE."
            ),
            "{cql}"
        );
    }
    let e = h.err("TRUNCATE ks.u USING TIMEOUT 0s").await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            ErrorCode::Truncate,
            "Error during truncate: std::runtime_error (Timeout during TRUNCATE TABLE of ks.u)"
        )
    );
    // ScyllaDB doesn't count a TRUNCATE's marker.
    assert!(
        h.s.prepare("TRUNCATE ks.u USING TIMEOUT ?", None)
            .await
            .unwrap()
            .bind
            .is_empty()
    );
    // Batches: a single partition times out as a plain write, a logged one in its log.
    h.ok("CREATE TABLE ks.c (p int PRIMARY KEY, n counter)")
        .await;
    for (cql, table, ty) in [
        (
            "BEGIN UNLOGGED BATCH USING TIMEOUT 0s INSERT INTO ks.u (p, v) VALUES (1, 1); INSERT INTO ks.u (p, v) VALUES (2, 1); APPLY BATCH",
            "ks.u",
            "UNLOGGED_BATCH",
        ),
        (
            "BEGIN BATCH USING TIMEOUT 0s INSERT INTO ks.u (p, v) VALUES (1, 1); INSERT INTO ks.u (p, v) VALUES (2, 1); APPLY BATCH",
            "system.batchlog_v2",
            "BATCH_LOG",
        ),
        (
            "BEGIN UNLOGGED BATCH USING TIMEOUT 0s INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH",
            "ks.u",
            "SIMPLE",
        ),
        (
            "BEGIN COUNTER BATCH USING TIMEOUT 0s UPDATE ks.c SET n = n + 1 WHERE p = 1; APPLY BATCH",
            "ks.c",
            "COUNTER",
        ),
        (
            "UPDATE ks.c USING TIMEOUT 0s SET n = n + 1 WHERE p = 1",
            "ks.c",
            "COUNTER",
        ),
    ] {
        let e = h.err(cql).await;
        assert_eq!(
            e.message,
            format!("Operation timed out for {table} - received only 0 responses from 1 CL=ONE."),
            "{cql}"
        );
        assert!(
            matches!(&e.detail, crate::error::ErrorDetail::WriteTimeout { write_type, received: 0, block_for: 1, .. } if write_type == ty),
            "{cql}: {:?}",
            e.detail
        );
    }
    // Reads and conditional writes take no notice.
    h.ok("SELECT * FROM ks.u WHERE p = 1 USING TIMEOUT 0ms")
        .await;
    h.ok("INSERT INTO ks.u (p, v) VALUES (1, 1) IF NOT EXISTS USING TIMEOUT 0s")
        .await;
    // Bound values.
    for (cql, v, want) in [
        (
            "SELECT * FROM ks.u USING TIMEOUT ?",
            duration(1, 0, 0),
            Some(days),
        ),
        (
            "SELECT * FROM ks.u USING TIMEOUT ?",
            duration(0, 1, 0),
            Some(days),
        ),
        (
            "SELECT * FROM ks.u USING TIMEOUT ?",
            duration(0, 0, 1000),
            Some(fine),
        ),
        (
            "SELECT * FROM ks.u USING TIMEOUT ?",
            duration(0, 0, -1_000_000),
            Some(neg),
        ),
        (
            "SELECT * FROM ks.u USING TIMEOUT ?",
            duration(0, 0, 5_000_000),
            None,
        ),
        (
            "BEGIN BATCH USING TIMEOUT ? INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH",
            duration(1, 0, 0),
            Some(days),
        ),
        (
            "BEGIN BATCH USING TIMEOUT ? INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH",
            duration(0, 0, 5_000_000),
            None,
        ),
        (
            "SELECT * FROM ks.u USING TIMEOUT ?",
            Param::Null,
            Some("Timeout value cannot be null"),
        ),
        (
            "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMEOUT ?",
            Param::Null,
            Some("Timeout value cannot be null"),
        ),
        // A statement's own timeout in a batch is ignored.
        (
            "BEGIN BATCH INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMEOUT ?; APPLY BATCH",
            duration(1, 0, 0),
            None,
        ),
    ] {
        let p =
            h.s.prepare(cql, None)
                .await
                .unwrap_or_else(|e| panic!("{cql}: {e}"));
        match (
            h.s.execute(&p.id, QueryParams::with_values(vec![v])).await,
            want,
        ) {
            (Ok(_), None) => {}
            (Err(e), Some(w)) => assert_eq!(e.message, w, "{cql}"),
            (r, w) => panic!("{cql}: {:?}, want {w:?}", r.map(|r| r.body)),
        }
    }
}

#[tokio::test]
async fn future_timestamps() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.u (p int PRIMARY KEY, v int)"
    ))
    .await;
    let want = "Cannot provide a timestamp more than 3 days into the future. If this was not intended, make sure the timestamp is in microseconds. You can also disable this check by setting the restrict_future_timestamp configuration option to false.";
    let soon = crate::rowttl::now_millis() * 1000 + 2 * 86_400_000_000;
    for cql in [
        "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMESTAMP 99999999999999999".to_string(),
        "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMESTAMP 9223372036854775807".to_string(),
        "UPDATE ks.u USING TIMESTAMP 99999999999999999 SET v = 1 WHERE p = 1".to_string(),
        "DELETE FROM ks.u USING TIMESTAMP 99999999999999999 WHERE p = 1".to_string(),
        "BEGIN BATCH USING TIMESTAMP 99999999999999999 INSERT INTO ks.u (p, v) VALUES (1, 1); APPLY BATCH".to_string(),
        "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMESTAMP 1876543210000000".to_string(),
    ] {
        assert_eq!(h.msg(&cql).await, want, "{cql}");
    }
    h.ok(&format!(
        "INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMESTAMP {soon}"
    ))
    .await;
    // The switch.
    h.ok("UPDATE system.config SET value = 'false' WHERE name = 'restrict_future_timestamp'")
        .await;
    h.ok("INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMESTAMP 99999999999999999")
        .await;
    // Cassandra accepts them.
    let mut c = H::new(Dialect::Cassandra).await;
    c.exec(&format!(
        "{KS};\nCREATE TABLE ks.u (p int PRIMARY KEY, v int)"
    ))
    .await;
    c.ok("INSERT INTO ks.u (p, v) VALUES (1, 1) USING TIMESTAMP 99999999999999999")
        .await;
}
