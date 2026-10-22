//! Size and count limits probed on both references: clustering key length (64 KiB),
//! ScyllaDB's batch size failure, its relation count, vector dimension, index name length, the
//! index key length of an indexed read.

use super::*;

fn text_param(n: usize) -> Param {
    Param::new(Bytes::from(vec![b'x'; n]))
}

async fn exec(h: &mut H, cql: &str, values: Vec<Param>) -> Result<Response, Error> {
    let p =
        h.s.prepare(cql, None)
            .await
            .unwrap_or_else(|e| panic!("{cql}: {e}"));
    h.s.execute(&p.id, QueryParams::with_values(values)).await
}

#[tokio::test]
async fn clustering_key_length() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!("{KS};\nCREATE TABLE ks.t (p int, c text, c2 text, v text, PRIMARY KEY (p, c, c2));\nCREATE TABLE ks.one (p int, c text, PRIMARY KEY (p, c))")).await;
        let scylla = d.is_scylla();
        for (cql, values, scylla_len, cassandra_len) in [
            (
                "INSERT INTO ks.t (p, c, c2, v) VALUES (1, ?, 'a', 'x')",
                vec![text_param(65536)],
                Some(65541),
                Some(65537),
            ),
            (
                "INSERT INTO ks.t (p, c, c2, v) VALUES (1, ?, 'a', 'x')",
                vec![text_param(65535)],
                Some(65540),
                Some(65536),
            ),
            (
                "INSERT INTO ks.t (p, c, c2, v) VALUES (1, ?, ?, 'x')",
                vec![text_param(40000), text_param(40000)],
                Some(80004),
                Some(80000),
            ),
            (
                "UPDATE ks.t SET v = 'x' WHERE p = 1 AND c = ? AND c2 = 'a'",
                vec![text_param(65536)],
                Some(65541),
                Some(65537),
            ),
            (
                "DELETE FROM ks.t WHERE p = 1 AND c = ? AND c2 = 'a'",
                vec![text_param(65536)],
                Some(65541),
                Some(65537),
            ),
            (
                "SELECT * FROM ks.t WHERE p = 1 AND c = ? AND c2 = 'a'",
                vec![text_param(65536)],
                Some(65541),
                None,
            ),
            // A composite key of 65535 - 4 bytes fits.
            (
                "INSERT INTO ks.t (p, c, c2, v) VALUES (1, ?, ?, 'x')",
                vec![text_param(100), text_param(65431)],
                None,
                None,
            ),
            // ScyllaDB refuses a single 65535-byte component (its issue #16772); roll doesn't.
            (
                "INSERT INTO ks.one (p, c) VALUES (1, ?)",
                vec![text_param(65535)],
                None,
                None,
            ),
            (
                "INSERT INTO ks.one (p, c) VALUES (1, ?)",
                vec![text_param(65536)],
                Some(65538),
                Some(65536),
            ),
        ] {
            let want = if scylla { scylla_len } else { cassandra_len };
            match (exec(&mut h, cql, values).await, want) {
                (Ok(_), None) => {}
                (Err(e), Some(n)) => assert_eq!(
                    (e.code, e.message),
                    (
                        ErrorCode::Invalid,
                        format!("Key length of {n} is longer than maximum of 65535")
                    ),
                    "{d}: {cql}"
                ),
                (r, w) => panic!("{d}: {cql}: got {:?}, want {w:?}", r.map(|r| r.body)),
            }
        }
    }
}

#[tokio::test]
async fn scylla_limits() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.k (k int PRIMARY KEY, t text, v int)"
    ))
    .await;
    // A multi-partition batch over 1 MiB fails (by the mutations' footprint); one partition
    // doesn't; nothing warns.
    let batch = |n: usize, same: bool| {
        let stmts: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "INSERT INTO ks.k (k, t) VALUES ({}, '{}')",
                    if same { 1 } else { i },
                    "x".repeat(743)
                )
            })
            .collect();
        format!("BEGIN BATCH\n{}\n APPLY BATCH\n", stmts.join("\n"))
    };
    assert_eq!(h.msg(&batch(1025, false)).await, "Batch too large");
    assert!(h.ok(&batch(1024, false)).await.warnings.is_empty());
    assert!(h.ok(&batch(1025, true)).await.warnings.is_empty());
    // At most 100 relations.
    let rel = |n: usize| {
        format!(
            "SELECT * FROM ks.k WHERE k = 1{} ALLOW FILTERING",
            " AND v = 1".repeat(n - 1)
        )
    };
    h.ok(&rel(100)).await;
    let e = h.err(&rel(101)).await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::Syntax, "too many relations in WHERE clause")
    );
    // Vectors up to 16000 dimensions.
    assert_eq!(
        h.msg("CREATE TABLE ks.v (p int PRIMARY KEY, v vector<float, 16001>)")
            .await,
        "Vectors must have a dimension less than or equal to 16000"
    );
    h.ok("CREATE TABLE ks.v (p int PRIMARY KEY, v vector<float, 16000>)")
        .await;
    // Index names up to 192 characters.
    let long = "x".repeat(193);
    assert_eq!(
        h.msg(&format!("CREATE INDEX {long} ON ks.k (v)")).await,
        format!("index names shouldn't be more than 192 characters long (got \"{long}\")")
    );
    h.ok(&format!("CREATE INDEX {} ON ks.k (v)", "x".repeat(183)))
        .await;
    // An indexed read of an oversized value.
    h.ok("CREATE INDEX ON ks.k (t)").await;
    let e = exec(
        &mut h,
        "SELECT k FROM ks.k WHERE t = ?",
        vec![text_param(66536)],
    )
    .await
    .unwrap_err();
    assert_eq!(
        e.message,
        "Key length of 66538 is longer than maximum of 65535"
    );
    // Cassandra has no such limits.
    let mut c = H::new(Dialect::Cassandra).await;
    c.exec(&format!(
        "{KS};\nCREATE TABLE ks.v (p int PRIMARY KEY, v vector<float, 16001>)"
    ))
    .await;
}
