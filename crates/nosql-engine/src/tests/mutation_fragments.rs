//! `SELECT ... FROM MUTATION_FRAGMENTS(t)` ([`crate::mutation_fragments`]): the derived schema,
//! fragments per source (memtable, row cache and SSTables after a flush), the JSON of each
//! fragment kind, restrictions, aggregates and paging, and the errors, as probed on the
//! reference (ScyllaDB 2026.3.3).

use super::*;

async fn setup() -> H {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, s int static, m map<int, text>, PRIMARY KEY (pk, ck))"
    ))
    .await;
    h
}

const COLS: &str = "pk, mutation_source, partition_region, ck, position_weight, mutation_fragment_kind, metadata, value";

#[tokio::test]
async fn fragments_of_each_kind() {
    let mut h = setup().await;
    h.exec(
        "DELETE FROM ks.t USING TIMESTAMP 10 WHERE pk = 0;\n\
         DELETE FROM ks.t USING TIMESTAMP 11 WHERE pk = 0 AND ck > 0 AND ck < 2;\n\
         INSERT INTO ks.t (pk, ck, v) VALUES (0, 0, 0) USING TIMESTAMP 12;\n\
         INSERT INTO ks.t (pk, ck, v, s, m) VALUES (0, 1, 0, 5, {1: 'a', 2: 'b'}) USING TIMESTAMP 13;\n\
         INSERT INTO ks.t (pk, ck, v) VALUES (1, 0, 0) USING TIMESTAMP 14;\n\
         UPDATE ks.t USING TIMESTAMP 15 SET v = null WHERE pk = 1 AND ck = 5",
    )
    .await;
    let rows = h
        .rows(&format!("SELECT {COLS} FROM MUTATION_FRAGMENTS(ks.t)"))
        .await;
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| {
                    // Deletion times are the write's wall clock.
                    let mut out = String::new();
                    let mut rest = c.as_str();
                    while let Some(i) = rest.find("\"deletion_time\":\"") {
                        out.push_str(&rest[..i + 17]);
                        rest = &rest[i + 17..];
                        let end = rest.find('"').unwrap_or(0);
                        out.push('D');
                        rest = &rest[end..];
                    }
                    out.push_str(rest);
                    out
                })
                .collect()
        })
        .collect();
    let want = [
        [
            "1",
            "memtable:0",
            "0",
            "",
            "",
            "partition start",
            r#"{"tombstone":{}}"#,
            "null",
        ],
        [
            "1",
            "memtable:0",
            "2",
            "0",
            "0",
            "clustering row",
            r#"{"marker":{"timestamp":14},"columns":{"v":{"is_live":true,"type":"regular","timestamp":14}}}"#,
            r#"{"v":"0"}"#,
        ],
        [
            "1",
            "memtable:0",
            "2",
            "5",
            "0",
            "clustering row",
            r#"{"columns":{"v":{"is_live":false,"type":"regular","timestamp":15,"deletion_time":"D"}}}"#,
            r#"{"v":null}"#,
        ],
        [
            "1",
            "memtable:0",
            "3",
            "",
            "",
            "partition end",
            "null",
            "null",
        ],
        [
            "0",
            "memtable:0",
            "0",
            "",
            "",
            "partition start",
            r#"{"tombstone":{"timestamp":10,"deletion_time":"D"}}"#,
            "null",
        ],
        [
            "0",
            "memtable:0",
            "1",
            "",
            "",
            "static row",
            r#"{"s":{"is_live":true,"type":"regular","timestamp":13}}"#,
            r#"{"s":"5"}"#,
        ],
        [
            "0",
            "memtable:0",
            "2",
            "0",
            "0",
            "clustering row",
            r#"{"marker":{"timestamp":12},"columns":{"v":{"is_live":true,"type":"regular","timestamp":12}}}"#,
            r#"{"v":"0"}"#,
        ],
        [
            "0",
            "memtable:0",
            "2",
            "0",
            "1",
            "range tombstone change",
            r#"{"tombstone":{"timestamp":11,"deletion_time":"D"}}"#,
            "null",
        ],
        [
            "0",
            "memtable:0",
            "2",
            "1",
            "0",
            "clustering row",
            r#"{"marker":{"timestamp":13},"columns":{"m":{"tombstone":{"timestamp":12,"deletion_time":"D"},"cells":[{"key":"1","value":{"is_live":true,"type":"regular","timestamp":13}},{"key":"2","value":{"is_live":true,"type":"regular","timestamp":13}}]},"v":{"is_live":true,"type":"regular","timestamp":13}}}"#,
            r#"{"m":[{"key":"1","value":"a"},{"key":"2","value":"b"}],"v":"0"}"#,
        ],
        [
            "0",
            "memtable:0",
            "2",
            "2",
            "-1",
            "range tombstone change",
            r#"{"tombstone":{}}"#,
            "null",
        ],
        [
            "0",
            "memtable:0",
            "3",
            "",
            "",
            "partition end",
            "null",
            "null",
        ],
    ];
    // Missing clustering components are empty values (the driver reads an empty int as null).
    assert_eq!(rows.len(), want.len(), "{rows:#?}");
    for (got, want) in rows.iter().zip(want) {
        assert_eq!(got, &want.map(String::from).to_vec());
    }
    // The derived schema.
    assert_eq!(
        h.specs("SELECT * FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1")
            .await,
        [
            ("pk", "int"),
            ("mutation_source", "text"),
            ("partition_region", "tinyint"),
            ("ck", "int"),
            ("position_weight", "tinyint"),
            ("metadata", "text"),
            ("mutation_fragment_kind", "text"),
            ("value", "text")
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .to_vec()
    );
    let r = h
        .ok("SELECT * FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1")
        .await;
    assert_eq!(
        r.rows().unwrap().metadata.columns[0].table,
        "t_$mutation_fragments"
    );
}

#[tokio::test]
async fn sources_restrictions_and_errors() {
    let mut h = setup().await;
    h.exec("INSERT INTO ks.t (pk, ck, v) VALUES (1, 0, 0);\nINSERT INTO ks.t (pk, ck, v) VALUES (1, 5, 5)").await;
    let t = h.db.schema().keyspace("ks").unwrap().tables["t"].clone();
    h.db.storage_table(&t).unwrap().flush().await.unwrap();
    h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (1, 7, 7)").await;
    let sources: Vec<String> = h
        .rows("SELECT mutation_source FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1")
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(sources.len(), 3 + 4 + 4, "{sources:?}");
    assert_eq!(
        &sources[..4],
        ["memtable:0"; 3]
            .iter()
            .chain(["row-cache"].iter())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        sources[7].starts_with("sstable:/var/lib/scylla/data/ks/t-")
            && sources[7].ends_with("-big-Data.db"),
        "{sources:?}"
    );
    // Clustering restrictions on the derived key; the base's clustering needs the prefix.
    assert_eq!(
        h.rows("SELECT ck FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1 AND mutation_source = 'row-cache' AND partition_region = 2 AND ck > 0").await,
        table(&[&["5"]])
    );
    assert_eq!(
        h.msg("SELECT * FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1 AND ck = 0")
            .await,
        "PRIMARY KEY column \"ck\" cannot be restricted as preceding column \"mutation_source\" is not restricted"
    );
    assert_eq!(
        h.rows("SELECT ck FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1 AND ck = 0 ALLOW FILTERING")
            .await
            .len(),
        2
    );
    assert_eq!(
        h.rows("SELECT count(*) FROM MUTATION_FRAGMENTS(ks.t)")
            .await,
        table(&[&["11"]])
    );
    assert_eq!(
        h.rows("SELECT mutation_source, count(*) FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1 GROUP BY pk, mutation_source").await.len(),
        3
    );
    assert_eq!(
        h.rows("SELECT count(*) FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1 AND mutation_fragment_kind = 'clustering row' ALLOW FILTERING").await,
        table(&[&["5"]])
    );
    assert!(
        h.rows("SELECT * FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 99")
            .await
            .is_empty()
    );
    // Paging returns every fragment once.
    let mut all = Vec::new();
    let mut ps: Option<Bytes> = None;
    loop {
        let p = QueryParams {
            page_size: Some(2),
            paging_state: ps.clone(),
            ..Default::default()
        };
        let (rows, next) = h.rows_with("SELECT mutation_source, partition_region, ck, position_weight FROM MUTATION_FRAGMENTS(ks.t)", p).await;
        all.extend(rows);
        match next {
            Some(n) => ps = Some(n),
            None => break,
        }
    }
    assert_eq!(all.len(), 11);
    for (cql, want) in [
        (
            "SELECT * FROM MUTATION_FRAGMENTS(ks.nope)",
            "unconfigured table nope",
        ),
        (
            "SELECT * FROM MUTATION_FRAGMENTS(nope.t)",
            "Keyspace nope does not exist",
        ),
        (
            "SELECT * FROM MUTATION_FRAGMENTS(t)",
            "No keyspace has been specified. USE a keyspace, or explicitly specify keyspace.tablename",
        ),
        (
            "SELECT * FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 0 ORDER BY mutation_source DESC",
            "ORDER BY is not supported in SELECT FROM MUTATION_FRAGMENTS() statements.",
        ),
    ] {
        assert_eq!(h.msg(cql).await, want, "{cql}");
    }
}

#[tokio::test]
async fn values_and_virtual_tables() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TYPE ks.u (a int, b text);\n\
         CREATE TABLE ks.mt (pk int, c1 int, c2 text, fl frozen<list<int>>, se set<int>, fu frozen<u>, tu tuple<int, text>, bl blob, ts timestamp, tx text, PRIMARY KEY (pk, c1, c2));\n\
         INSERT INTO ks.mt (pk, c1, c2, fl, se, fu, tu, bl, ts, tx) VALUES (1, 1, 'a', [1, 2], {{3, 4}}, {{a: 2, b: 'r'}}, (5, 's'), 0xcafe, '2020-01-01 00:00:00+0000', 'it''s \"q\"') USING TIMESTAMP 5;\n\
         DELETE FROM ks.mt USING TIMESTAMP 6 WHERE pk = 1 AND c1 = 2;\n\
         DELETE FROM ks.mt USING TIMESTAMP 7 WHERE pk = 1 AND c1 = 4 AND c2 = 'z'"
    ))
    .await;
    let rows = h
        .rows("SELECT c1, c2, position_weight, mutation_fragment_kind, metadata, value FROM MUTATION_FRAGMENTS(ks.mt) WHERE pk = 1 AND mutation_source = 'memtable:0' AND partition_region = 2")
        .await;
    assert_eq!(
        rows[0][5],
        r#"{"bl":"cafe","fl":"1, 2","fu":"2:r","se":[{"key":"3","value":""},{"key":"4","value":""}],"ts":"2020-01-01T00:00:00.000Z","tu":"5:s","tx":"it's \"q\""}"#
    );
    assert!(
        rows[0][4].contains(r#""fl":{"is_live":true,"type":"frozen-collection","timestamp":5}"#),
        "{}",
        rows[0][4]
    );
    // A prefix deletion: missing components are empty (null for an int, '' for a text).
    assert_eq!(
        &rows[1][..4],
        ["2", "", "-1", "range tombstone change"].map(String::from)
    );
    assert_eq!(
        &rows[2][..4],
        ["2", "", "1", "range tombstone change"].map(String::from)
    );
    assert!(
        rows[3][4].starts_with(r#"{"tombstone":{"timestamp":7,"#)
            && rows[3][4].contains(r#""shadowable_tombstone":{"timestamp":7,"#),
        "{}",
        rows[3][4]
    );
    assert_eq!(rows[3][5], "{}");
    // Virtual tables have one source.
    let r = h
        .rows("SELECT mutation_source, mutation_fragment_kind, value FROM MUTATION_FRAGMENTS(system.config) WHERE name = 'enable_tablets'")
        .await;
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], "virtual-table");
    assert!(r[1][2].starts_with(r#"{"source":"#), "{}", r[1][2]);
}

#[tokio::test]
async fn row_cache_and_sliced_range_tombstones() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (pk int, c1 int, c2 int, v int, PRIMARY KEY (pk, c1, c2))"
    ))
    .await;
    let t = h.db.schema().keyspace("ks").unwrap().tables["t"].clone();
    let st = h.db.storage_table(&t).unwrap();
    h.ok("INSERT INTO ks.t (pk, c1, c2, v) VALUES (1, 0, 0, 0)")
        .await;
    st.flush().await.unwrap();
    let sources = |rows: Vec<Vec<String>>| {
        let mut s: Vec<String> = rows
            .into_iter()
            .map(|r| r[0].split(':').next().unwrap_or("").to_string())
            .collect();
        s.dedup();
        s
    };
    let q = "SELECT mutation_source FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1";
    assert_eq!(sources(h.rows(q).await), ["row-cache", "sstable"]);
    // Dropped caches hold a partition again once it is read, or flushed again.
    h.db.drop_row_cache();
    assert_eq!(sources(h.rows(q).await), ["sstable"]);
    h.ok("SELECT * FROM ks.t WHERE pk = 1 BYPASS CACHE").await;
    assert_eq!(sources(h.rows(q).await), ["sstable"]);
    h.ok("SELECT * FROM ks.t WHERE pk = 1").await;
    assert_eq!(sources(h.rows(q).await), ["row-cache", "sstable"]);
    h.db.drop_row_cache();
    h.ok("INSERT INTO ks.t (pk, c1, c2, v) VALUES (1, 5, 5, 5)")
        .await;
    st.flush().await.unwrap();
    assert_eq!(sources(h.rows(q).await), ["row-cache", "sstable"]);
    // Range tombstones cut at the bounds of a sliced read.
    h.exec(
        "DELETE FROM ks.t WHERE pk = 2 AND c1 = 0;\n\
         DELETE FROM ks.t WHERE pk = 2 AND c1 = 0 AND c2 > 10 AND c2 < 20;\n\
         DELETE FROM ks.t WHERE pk = 2 AND c1 = 0 AND c2 > 20 AND c2 < 30",
    )
    .await;
    let slice = |a: i32, b: i32| {
        format!(
            "SELECT c1, c2, position_weight, metadata FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 2 AND mutation_source = 'memtable:0' AND partition_region = 2 AND c1 = 0 AND c2 >= {a} AND c2 < {b}"
        )
    };
    let rows = h.rows(&slice(0, 8)).await;
    assert_eq!(
        rows.iter()
            .map(|r| (r[1].clone(), r[2].clone()))
            .collect::<Vec<_>>(),
        [("0".into(), "-1".into()), ("8".into(), "-1".into())]
    );
    assert!(
        rows[0][3].starts_with("{\"tombstone\":{\"timestamp\":")
            && rows[1][3] == "{\"tombstone\":{}}",
        "{rows:?}"
    );
    // Inside the slice: its own changes, then the cut.
    let rows = h.rows(&slice(5, 15)).await;
    assert_eq!(
        rows.iter()
            .map(|r| (r[1].clone(), r[2].clone()))
            .collect::<Vec<_>>(),
        [
            ("5".into(), "-1".into()),
            ("10".into(), "1".into()),
            ("15".into(), "-1".into())
        ]
    );
    // A change exactly at the slice's start: the tombstone before it, then the change.
    let rows = h.rows(&slice(30, 38)).await;
    assert_eq!(rows.iter().map(|r| r[1].clone()).collect::<Vec<_>>(), ["30", "30", "38"]);
}
