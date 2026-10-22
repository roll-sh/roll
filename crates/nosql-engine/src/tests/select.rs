//! Reads: paging continuation in every mode, IN ordering, filtering, aggregates over pages.
//! Results are checked against the same statement unpaged (the references page the same rows;
//! first pages are in the captured suites).

use super::*;

async fn fixture(d: Dialect) -> H {
    let mut h = H::new(d).await;
    h.ok(KS).await;
    h.ok("CREATE TABLE ks.p (pk int, ck int, v int, s int static, PRIMARY KEY (pk, ck))")
        .await;
    for pk in 0..6 {
        for ck in 0..4 {
            h.ok(&format!(
                "INSERT INTO ks.p (pk, ck, v) VALUES ({pk}, {ck}, {})",
                pk * 10 + ck
            ))
            .await;
        }
    }
    // A partition with only a static row is one result row.
    h.ok("INSERT INTO ks.p (pk, s) VALUES (10, 7)").await;
    h
}

/// Every page of `cql` at `page_size`, following the paging state to the end.
async fn walk(h: &mut H, cql: &str, page_size: i32) -> Vec<Vec<String>> {
    let mut all = Vec::new();
    let mut state = None;
    for _ in 0..1000 {
        let p = QueryParams {
            page_size: Some(page_size),
            paging_state: state.take(),
            ..Default::default()
        };
        let (rows, next) = h.rows_with(cql, p).await;
        // ScyllaDB returns every group in one page (`captured_masking`: `reads/pg_group_*`).
        let one_page = h.db.dialect().is_scylla() && cql.contains("GROUP BY");
        assert!(
            one_page || rows.len() <= page_size as usize,
            "{cql}: page of {} > {page_size}",
            rows.len()
        );
        all.extend(rows);
        match next {
            Some(s) => state = Some(s),
            None => return all,
        }
    }
    panic!("{cql}: paging does not end");
}

#[tokio::test]
async fn paging_returns_every_row_once() {
    for d in BOTH {
        let mut h = fixture(d).await;
        for cql in [
            "SELECT pk, ck, v, s FROM ks.p",
            "SELECT pk, ck FROM ks.p LIMIT 7",
            "SELECT pk, ck FROM ks.p PER PARTITION LIMIT 2",
            "SELECT pk, ck FROM ks.p PER PARTITION LIMIT 1 LIMIT 3",
            "SELECT pk, ck FROM ks.p WHERE pk IN (3, 1, 2)",
            "SELECT pk, ck FROM ks.p WHERE pk = 1 ORDER BY ck DESC",
            "SELECT pk, ck FROM ks.p WHERE pk = 2 AND ck > 0",
            "SELECT pk, ck FROM ks.p WHERE v > 15 ALLOW FILTERING",
            "SELECT DISTINCT pk FROM ks.p",
            "SELECT pk FROM ks.p WHERE token(pk) > 0",
        ] {
            let full = h.rows(cql).await;
            assert!(!full.is_empty(), "{d}: {cql}");
            for size in [1, 2, 3, 5, 100] {
                assert_eq!(walk(&mut h, cql, size).await, full, "{d}: {cql} by {size}");
            }
        }
    }
}

#[tokio::test]
async fn aggregates_and_groups_ignore_the_page_size() {
    for d in BOTH {
        let mut h = fixture(d).await;
        for cql in [
            "SELECT count(*), sum(v) FROM ks.p",
            "SELECT pk, count(*), max(v) FROM ks.p GROUP BY pk",
            "SELECT pk, ck FROM ks.p WHERE pk = 3 GROUP BY pk, ck",
        ] {
            let full = h.rows(cql).await;
            assert_eq!(walk(&mut h, cql, 2).await, full, "{d}: {cql}");
        }
        assert_eq!(
            h.rows("SELECT count(*), min(v), max(v) FROM ks.p WHERE pk = 1")
                .await,
            table(&[&["4", "10", "13"]])
        );
    }
}

#[tokio::test]
async fn static_only_partitions_and_in_order() {
    for d in BOTH {
        let mut h = fixture(d).await;
        assert_eq!(
            h.rows("SELECT pk, ck, s FROM ks.p WHERE pk = 10").await,
            table(&[&["10", "null", "7"]])
        );
        // IN keys are read in partition key order, whatever the order written.
        assert_eq!(
            h.rows("SELECT pk FROM ks.p WHERE pk IN (5, 0) AND ck = 0")
                .await,
            table(&[&["0"], &["5"]])
        );
    }
}

#[tokio::test]
async fn garbage_paging_state_is_a_protocol_error() {
    for d in BOTH {
        let mut h = fixture(d).await;
        let p = QueryParams {
            page_size: Some(2),
            paging_state: Some(Bytes::from_static(b"\x07garbage")),
            ..Default::default()
        };
        let e = h.s.query("SELECT * FROM ks.p", p).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Protocol, "{d}");
    }
}

#[tokio::test]
async fn ttl_expires_at_now_in_seconds() {
    for d in BOTH {
        let mut h = fixture(d).await;
        h.ok("INSERT INTO ks.p (pk, ck, v) VALUES (20, 1, 1) USING TTL 100")
            .await;
        let ttl = h.rows("SELECT ttl(v) FROM ks.p WHERE pk = 20").await;
        let left: i32 = ttl[0][0].parse().unwrap();
        assert!((98..=100).contains(&left), "{d}: {left}");
        let later = QueryParams {
            now_in_seconds: Some(4_000_000_000),
            ..Default::default()
        };
        let (rows, _) = h.rows_with("SELECT * FROM ks.p WHERE pk = 20", later).await;
        assert!(rows.is_empty(), "{d}: {rows:?}");
    }
}

#[tokio::test]
async fn result_metadata_names_and_types() {
    for d in BOTH {
        let mut h = fixture(d).await;
        assert_eq!(
            h.columns("SELECT v AS x, writetime(v), ttl(v), count(*) FROM ks.p WHERE pk = 1")
                .await,
            ["x", "writetime(v)", "ttl(v)", "count"],
            "{d}"
        );
        assert_eq!(
            h.specs("SELECT token(pk), CAST(v AS text) FROM ks.p WHERE pk = 1")
                .await,
            [
                ("system.token(pk)".to_string(), "bigint".to_string()),
                ("cast(v as text)".to_string(), "text".to_string())
            ],
            "{d}"
        );
        // Unknown columns are reported in each dialect's words.
        let m = h.msg("SELECT nope FROM ks.p").await;
        assert_eq!(
            m,
            if d.is_scylla() {
                "Unrecognized name nope"
            } else {
                "Undefined column name nope in table ks.p"
            }
        );
    }
}

/// With GROUP BY, PER PARTITION LIMIT counts groups (`SelectGroupByTest`, both references).
#[tokio::test]
async fn per_partition_limit_counts_groups() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.ok(KS).await;
        h.ok("CREATE TABLE ks.g (a int, b int, c int, d int, PRIMARY KEY (a, b, c))")
            .await;
        for i in 1..5 {
            for j in 1..5 {
                for k in 1..5 {
                    h.ok(&format!(
                        "INSERT INTO ks.g (a, b, c, d) VALUES ({i}, {j}, {k}, {})",
                        i + j
                    ))
                    .await;
                }
            }
        }
        h.ok("DELETE FROM ks.g WHERE a = 3").await;
        let cases: &[(&str, &[&[&str]])] = &[
            (
                "SELECT a, b, d, count(b), max(d) FROM ks.g WHERE b IN (1, 2) AND c IN (1, 2) GROUP BY a, b PER PARTITION LIMIT 1 ALLOW FILTERING",
                &[
                    &["1", "1", "2", "2", "2"],
                    &["2", "1", "3", "2", "3"],
                    &["4", "1", "5", "2", "5"],
                ],
            ),
            (
                "SELECT a, b, d, count(b), max(d) FROM ks.g WHERE b IN (1, 2) AND c IN (1, 2) GROUP BY a, b PER PARTITION LIMIT 1 LIMIT 2 ALLOW FILTERING",
                &[&["1", "1", "2", "2", "2"], &["2", "1", "3", "2", "3"]],
            ),
            (
                "SELECT a, b, count(*) FROM ks.g GROUP BY a, b PER PARTITION LIMIT 2",
                &[
                    &["1", "1", "4"],
                    &["1", "2", "4"],
                    &["2", "1", "4"],
                    &["2", "2", "4"],
                    &["4", "1", "4"],
                    &["4", "2", "4"],
                ],
            ),
            (
                "SELECT a, b, count(*) FROM ks.g WHERE a = 2 GROUP BY a, b PER PARTITION LIMIT 3",
                &[&["2", "1", "4"], &["2", "2", "4"], &["2", "3", "4"]],
            ),
        ];
        for (cql, want) in cases {
            let mut got = h.rows(cql).await;
            got.sort();
            assert_eq!(got, table(want), "{d}: {cql}");
            let mut paged = walk(&mut h, cql, 1).await;
            paged.sort();
            assert_eq!(paged, table(want), "{d}: {cql} paged");
        }
    }
}

/// PER PARTITION LIMIT with static-only partitions, at every page size
/// (`SelectLimitTest.testPerPartitionLimitWithStaticDataAndPaging`).
#[tokio::test]
async fn per_partition_limit_with_static_rows_and_paging() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.ok(KS).await;
        h.ok("CREATE TABLE ks.s (a int, b int, s int static, c int, PRIMARY KEY (a, b))")
            .await;
        for i in 0..5 {
            h.ok(&format!("INSERT INTO ks.s (a, s) VALUES ({i}, {i})"))
                .await;
        }
        let statics = |ks: &[i32]| -> Vec<Vec<String>> {
            ks.iter()
                .map(|k| vec![k.to_string(), "null".into(), k.to_string(), "null".into()])
                .collect()
        };
        let cases: &[(&str, &[i32])] = &[
            ("SELECT * FROM ks.s PER PARTITION LIMIT 2", &[0, 1, 2, 3, 4]),
            (
                "SELECT * FROM ks.s PER PARTITION LIMIT 2 LIMIT 4",
                &[0, 1, 2, 3],
            ),
            (
                "SELECT * FROM ks.s PER PARTITION LIMIT 2 LIMIT 3",
                &[0, 1, 2],
            ),
            (
                "SELECT * FROM ks.s WHERE a IN (1, 3, 4) PER PARTITION LIMIT 2",
                &[1, 3, 4],
            ),
            (
                "SELECT * FROM ks.s WHERE a IN (1, 3, 4) PER PARTITION LIMIT 2 LIMIT 2",
                &[1, 3],
            ),
            ("SELECT * FROM ks.s WHERE a = 2 PER PARTITION LIMIT 3", &[2]),
            (
                "SELECT * FROM ks.s WHERE a = 2 ORDER BY b DESC PER PARTITION LIMIT 3",
                &[2],
            ),
            (
                "SELECT * FROM ks.s WHERE a = 2 AND s > 0 PER PARTITION LIMIT 2 ALLOW FILTERING",
                &[2],
            ),
        ];
        // Ranges are in token order (1, 0, 2, 4, 3): LIMIT keeps the first ones of that order.
        let order = [1, 0, 2, 4, 3];
        for page in 1..8 {
            for (cql, keys) in cases {
                let got = walk(&mut h, cql, page).await;
                let mut want = keys.to_vec();
                if !cql.contains(" IN ") && !cql.contains("a = 2") {
                    want = order[..keys.len()].to_vec();
                }
                assert_eq!(got, statics(&want), "{d}: {cql} by {page}");
            }
        }
        for i in [0, 2, 3, 4] {
            for j in 0..5 {
                h.ok(&format!(
                    "INSERT INTO ks.s (a, b, s, c) VALUES ({i}, {j}, {i}, {j})"
                ))
                .await;
            }
        }
        for page in 1..8 {
            let mut got = walk(&mut h, "SELECT * FROM ks.s PER PARTITION LIMIT 2", page).await;
            got.sort();
            let mut want = statics(&[1]);
            for i in [0, 2, 3, 4] {
                for j in 0..2 {
                    want.push(vec![
                        i.to_string(),
                        j.to_string(),
                        i.to_string(),
                        j.to_string(),
                    ]);
                }
            }
            want.sort();
            assert_eq!(got, want, "{d}: by {page}");
            let got = walk(
                &mut h,
                "SELECT * FROM ks.s WHERE a IN (0, 1, 2) PER PARTITION LIMIT 1 LIMIT 2",
                page,
            )
            .await;
            assert_eq!(
                got,
                table(&[&["0", "0", "0", "0"], &["1", "null", "1", "null"]]),
                "{d}: by {page}"
            );
        }
    }
}

/// ScyllaDB ends a page at 1 MiB of rows whatever the page size; Cassandra counts rows only.
/// Neither pages with a page size of 0 (probed on both references: 15 rows of 100 kB).
#[tokio::test]
async fn scylla_pages_end_by_size() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.ok(KS).await;
        h.ok("CREATE TABLE ks.b (p int, c int, s text, PRIMARY KEY (p, c))")
            .await;
        let big = "x".repeat(100_000);
        for i in 0..15 {
            h.ok(&format!(
                "INSERT INTO ks.b (p, c, s) VALUES (0, {i}, '{big}')"
            ))
            .await;
        }
        for (ps, want) in [(100, vec![11usize, 4]), (0, vec![15])] {
            let mut pages = Vec::new();
            let mut state = None;
            loop {
                let p = QueryParams {
                    page_size: Some(ps),
                    paging_state: state.take(),
                    ..Default::default()
                };
                let (rows, next) = h.rows_with("SELECT * FROM ks.b", p).await;
                pages.push(rows.len());
                match next {
                    Some(s) => state = Some(s),
                    None => break,
                }
            }
            let want: Vec<usize> = if d.is_scylla() { want } else { vec![15] };
            assert_eq!(pages, want, "{d}: page size {ps}");
        }
    }
}

/// `ttl()` of elements and fields reports each cell's remaining TTL.
#[tokio::test]
async fn element_and_field_ttls() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TYPE ks.ut (a int, b int);\n\
             CREATE TABLE ks.t (p int PRIMARY KEY, m map<int, int>, s set<int>, u ut);\n\
             INSERT INTO ks.t (p, m, s, u) VALUES (1, {{1: 10}}, {{1}}, {{a: 1}}) USING TTL 1000;\n\
             UPDATE ks.t USING TTL 5000 SET m[2] = 20, s = s + {{2}}, u.b = 2 WHERE p = 1"
        ))
        .await;
        let r = h
            .rows("SELECT ttl(m[1]), ttl(m[2]), ttl(s[1]), ttl(s[2]), ttl(u.a), ttl(u.b) FROM ks.t")
            .await;
        let ttls: Vec<i64> = r[0].iter().map(|v| v.parse().unwrap()).collect();
        for (i, want) in [1000, 5000, 1000, 5000, 1000, 5000].into_iter().enumerate() {
            assert!((want - 2..=want).contains(&ttls[i]), "{d}: {i}: {ttls:?}");
        }
    }
}
