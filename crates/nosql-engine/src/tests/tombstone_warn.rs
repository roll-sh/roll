//! Cassandra's tombstone warning (`statements::read_warn`), replayed from the reference capture
//! (`captured_tombstones.rs`): every page's warnings byte for byte and its row count, on
//! memtables and again after a flush. ScyllaDB answers the same reads without any.

use super::captured_tombstones::CASES;
use super::*;

/// The tables of `docs/nosql/reference/capture/tombstones.py`'s `setup` (keep in step).
async fn setup(h: &mut H) {
    let long = "x".repeat(600);
    let mut cql: Vec<String> = [
        "CREATE KEYSPACE gap_ WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}",
        "CREATE TABLE gap_.t (k int, c int, v int, s int static, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.u (\"Key\" text, k2 int, c1 int, c2 text, v text, w int, PRIMARY KEY ((\"Key\", k2), c1, c2)) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC)",
        "CREATE TABLE gap_.w (k int, c int, a int, b int, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.r (k int, c int, v int, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.p (k int PRIMARY KEY, v int, m map<text, int>, l list<int>)",
        "CREATE TABLE gap_.x (k bigint, c text, d uuid, b blob, v int, PRIMARY KEY (k, c, d))",
        "CREATE TABLE gap_.lng (k text, c int, v int, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.st (k int, c int, s int static, v int, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.f (k int, c int, v int, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.big (k int, c int, v int, PRIMARY KEY (k, c))",
        "CREATE TABLE gap_.col (k int, c int, s set<int>, m map<text, int>, v int, PRIMARY KEY (k, c))",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut a = |s: String| cql.push(s);
    for c in 0..1100 {
        a(format!("DELETE FROM gap_.t WHERE k = 1 AND c = {c}"));
        a(format!("DELETE FROM gap_.t WHERE k = 3 AND c = {c}"));
    }
    for c in 2000..2005 {
        a(format!("INSERT INTO gap_.t (k, c, v) VALUES (1, {c}, {c})"));
    }
    for c in 0..3 {
        a(format!("INSERT INTO gap_.t (k, c, v) VALUES (2, {c}, {c})"));
    }
    a("INSERT INTO gap_.t (k, c, v) VALUES (3, 2000, 1)".into());
    for c in 0..1100 {
        a(format!(
            "DELETE FROM gap_.u WHERE \"Key\" = 'it''s' AND k2 = 7 AND c1 = {c} AND c2 = 'x'"
        ));
    }
    a(
        "INSERT INTO gap_.u (\"Key\", k2, c1, c2, v, w) VALUES ('it''s', 7, 5000, 'a', 'hello', 1)"
            .into(),
    );
    for c in 0..1100 {
        a(format!(
            "INSERT INTO gap_.w (k, c, a, b) VALUES (1, {c}, 1, 1)"
        ));
        a(format!("DELETE a FROM gap_.w WHERE k = 1 AND c = {c}"));
    }
    for c in 0..600 {
        a(format!(
            "DELETE FROM gap_.r WHERE k = 1 AND c > {} AND c < {}",
            c * 10,
            c * 10 + 5
        ));
    }
    a("INSERT INTO gap_.r (k, c, v) VALUES (1, 100000, 1)".into());
    for k in 0..1100 {
        a(format!("DELETE v FROM gap_.p WHERE k = {k}"));
    }
    a("INSERT INTO gap_.p (k, v) VALUES (5000, 1)".into());
    for i in 0..1100 {
        a(format!(
            "DELETE FROM gap_.x WHERE k = 9 AND c = 'a''b' AND d = 00000000-0000-0000-0000-{i:012}"
        ));
    }
    a("INSERT INTO gap_.x (k, c, d, b, v) VALUES (9, 'zz', 00000000-0000-0000-0000-000000000001, 0xcafe, 1)".into());
    for c in 0..1100 {
        a(format!(
            "DELETE FROM gap_.lng WHERE k = '{long}' AND c = {c}"
        ));
    }
    a(format!(
        "INSERT INTO gap_.lng (k, c, v) VALUES ('{long}', 5000, 1)"
    ));
    a("INSERT INTO gap_.st (k, s) VALUES (1, 7)".into());
    a("INSERT INTO gap_.st (k, s) VALUES (2, 8)".into());
    for c in 0..1100 {
        a(format!("DELETE FROM gap_.st WHERE k = 1 AND c = {c}"));
    }
    for c in 0..40 {
        if c % 2 == 0 {
            a(format!("INSERT INTO gap_.f (k, c, v) VALUES (1, {c}, {c})"));
        } else {
            a(format!("DELETE FROM gap_.f WHERE k = 1 AND c = {c}"));
        }
    }
    for c in 100..1300 {
        a(format!("DELETE FROM gap_.f WHERE k = 1 AND c = {c}"));
    }
    a("INSERT INTO gap_.f (k, c, v) VALUES (1, 2000, 7)".into());
    for c in 0..1100 {
        a(format!("DELETE FROM gap_.f WHERE k = 3 AND c = {c}"));
    }
    for c in 0..10005 {
        a(format!("INSERT INTO gap_.big (k, c, v) VALUES (1, {c}, 1)"));
    }
    for c in 20000..21100 {
        a(format!("DELETE FROM gap_.big WHERE k = 1 AND c = {c}"));
    }
    a("INSERT INTO gap_.big (k, c, v) VALUES (1, 30000, 1)".into());
    for c in 0..1100 {
        a(format!("DELETE FROM gap_.col WHERE k = 1 AND c = {c}"));
    }
    a("INSERT INTO gap_.col (k, c, s, m, v) VALUES (1, 5000, {1, 2}, {'a': 1}, 3)".into());
    for s in &cql {
        h.ok(s).await;
    }
}

/// Every page of `cql`: (rows, warnings).
async fn pages(h: &mut H, cql: &str, page_size: Option<i32>) -> Vec<(usize, Vec<String>)> {
    try_pages(h, cql, page_size)
        .await
        .unwrap_or_else(|e| panic!("{cql}: {e}"))
}

async fn try_pages(
    h: &mut H,
    cql: &str,
    page_size: Option<i32>,
) -> Result<Vec<(usize, Vec<String>)>, Error> {
    let mut out = Vec::new();
    let mut paging_state = None;
    loop {
        let p = QueryParams {
            page_size,
            paging_state: paging_state.take(),
            ..Default::default()
        };
        let r = h.s.query(cql, p).await?;
        let ResponseBody::Rows(rows) = r.body else {
            panic!("{cql}: not rows")
        };
        out.push((rows.rows.len(), r.warnings));
        match rows.paging_state {
            Some(s) if out.len() < 100 => paging_state = Some(s),
            _ => return Ok(out),
        }
    }
}

#[tokio::test]
async fn captured_tombstone_warnings() {
    for d in [Dialect::Cassandra, Dialect::Scylla] {
        let mut h = H::new(d).await;
        setup(&mut h).await;
        let mut failures = Vec::new();
        for flushed in [false, true] {
            if flushed {
                h.db.flush().await.unwrap();
            }
            for (page_size, cql, want) in CASES {
                if d == Dialect::Scylla {
                    // Some of these ScyllaDB refuses.
                    let Ok(got) = try_pages(&mut h, cql, *page_size).await else {
                        continue;
                    };
                    for (_, w) in &got {
                        assert!(!w.iter().any(|w| w.contains("tombstone")), "{cql}: {w:?}");
                    }
                    continue;
                }
                let got = pages(&mut h, cql, *page_size).await;
                let want: Vec<(usize, Vec<String>)> = want
                    .iter()
                    .map(|(n, w)| (*n, w.iter().map(|s| s.to_string()).collect()))
                    .collect();
                if got != want {
                    failures.push(format!("{page_size:?} {cql} (flushed: {flushed})\n  got:  {got:?}\n  want: {want:?}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {}:\n{}",
            failures.len(),
            CASES.len() * 2,
            failures.join("\n")
        );
    }
}

/// An unpaged `IN` query reads its partitions one after the other here, so they warn in key
/// order (Cassandra's concurrent reads warn in completion order); the threshold applies live,
/// is exclusive, and system keyspaces never warn.
#[tokio::test]
async fn threshold_and_unpaged_in() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.exec(
        "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
         CREATE TABLE ks.t (k int, c int, v int, PRIMARY KEY (k, c))",
    )
    .await;
    for k in [1, 3] {
        for c in 0..5 {
            h.ok(&format!("DELETE FROM ks.t WHERE k = {k} AND c = {c}"))
                .await;
        }
        h.ok(&format!("INSERT INTO ks.t (k, c, v) VALUES ({k}, 9, 1)"))
            .await;
    }
    let warn = |k: i32, token: &str| {
        format!(
            "Read 1 live rows and 5 tombstone cells for query SELECT * FROM ks.t WHERE k = {k} ALLOW FILTERING; token {token} (see tombstone_warn_threshold)"
        )
    };
    let mut s = h.db.settings().as_ref().clone();
    s.tombstone_warn_threshold = 5;
    h.db.set_settings(s).unwrap();
    assert_eq!(
        pages(&mut h, "SELECT * FROM ks.t WHERE k IN (3, 1)", None).await,
        vec![(2, vec![])]
    );
    let mut s = h.db.settings().as_ref().clone();
    s.tombstone_warn_threshold = 4;
    h.db.set_settings(s).unwrap();
    assert_eq!(
        pages(&mut h, "SELECT * FROM ks.t WHERE k IN (3, 1)", None).await,
        vec![(
            2,
            vec![
                warn(1, "-4069959284402364209"),
                warn(3, "9010454139840013625")
            ]
        )]
    );
    s = h.db.settings().as_ref().clone();
    s.tombstone_warn_threshold = 0;
    h.db.set_settings(s).unwrap();
    let r = pages(
        &mut h,
        "SELECT * FROM system_schema.tables WHERE keyspace_name = 'ks'",
        None,
    )
    .await;
    assert!(r[0].1.is_empty(), "{r:?}");
}

/// Cell tombstones of one `DELETE` of 1001 map elements (`nosql-server/tests/compat/tombstones.cql`;
/// texts from cassandra:5.0.9), also through a prepared statement with bound values.
#[tokio::test]
async fn map_element_deletions() {
    let mut h = H::new(Dialect::Cassandra).await;
    let dels: Vec<String> = (0..1001).map(|i| format!("m['k{i}']")).collect();
    h.exec(&format!(
        "CREATE KEYSPACE gap_c WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}};
         CREATE TABLE gap_c.t (k int, c int, m map<text, int>, v int, PRIMARY KEY (k, c));
         INSERT INTO gap_c.t (k, c, v) VALUES (1, 1, 1);
         DELETE {} FROM gap_c.t WHERE k = 1 AND c = 2;
         INSERT INTO gap_c.t (k, c, v) VALUES (1, 3, 3);
         INSERT INTO gap_c.t (k, c, v) VALUES (2, 1, 1)",
        dels.join(", ")
    ))
    .await;
    let w = |live: u64, q: &str, token: &str| {
        format!(
            "Read {live} live rows and 1001 tombstone cells for query {q} ALLOW FILTERING; token {token} (see tombstone_warn_threshold)"
        )
    };
    let (t1, t2) = ("-4069959284402364209", "-3248873570005575792");
    let cases: Vec<(Option<i32>, &str, Vec<String>)> = vec![
        (
            None,
            "SELECT * FROM gap_c.t",
            vec![w(3, "SELECT * FROM gap_c.t", t2)],
        ),
        (
            Some(100),
            "SELECT * FROM gap_c.t",
            vec![w(3, "SELECT * FROM gap_c.t LIMIT 100", t2)],
        ),
        (
            None,
            "SELECT m FROM gap_c.t",
            vec![w(3, "SELECT m FROM gap_c.t", t2)],
        ),
        (
            None,
            "SELECT v FROM gap_c.t",
            vec![w(3, "SELECT v FROM gap_c.t", t2)],
        ),
        (
            None,
            "SELECT * FROM gap_c.t WHERE k IN (1, 2)",
            vec![w(2, "SELECT * FROM gap_c.t WHERE k = 1", t1)],
        ),
        (
            Some(100),
            "SELECT * FROM gap_c.t WHERE k = 1",
            vec![w(2, "SELECT * FROM gap_c.t WHERE k = 1 LIMIT 100", t1)],
        ),
        (
            Some(100),
            "SELECT v FROM gap_c.t WHERE k = 1 AND c >= 2",
            vec![w(
                1,
                "SELECT v FROM gap_c.t WHERE k = 1 AND c >= 2 LIMIT 100",
                t1,
            )],
        ),
        (
            Some(100),
            "SELECT * FROM gap_c.t WHERE k = 1 AND c > 2",
            vec![],
        ),
        (
            Some(100),
            "SELECT * FROM gap_c.t WHERE k = 1 LIMIT 1",
            vec![],
        ),
        (
            Some(100),
            "SELECT * FROM gap_c.t WHERE k = 1 LIMIT 2",
            vec![w(2, "SELECT * FROM gap_c.t WHERE k = 1 LIMIT 2", t1)],
        ),
        (
            Some(100),
            "SELECT * FROM gap_c.t WHERE k = 1 AND v = 3 ALLOW FILTERING",
            vec![w(
                2,
                "SELECT * FROM gap_c.t WHERE k = 1 AND v = 3 LIMIT 100",
                t1,
            )],
        ),
        (
            Some(100),
            "SELECT count(*) FROM gap_c.t WHERE k = 1",
            vec![w(2, "SELECT * FROM gap_c.t WHERE k = 1 LIMIT 100", t1)],
        ),
    ];
    for (page_size, cql, want) in cases {
        let got = pages(&mut h, cql, page_size).await;
        assert_eq!(got[0].1, want, "{cql}");
    }
    // Bound values print as literals.
    let p =
        h.s.prepare("SELECT v FROM gap_c.t WHERE k = ? AND c >= ?", None)
            .await
            .unwrap();
    let params = QueryParams {
        page_size: Some(100),
        ..QueryParams::with_values(vec![int(1), int(2)])
    };
    let r = h.s.execute(&p.id, params).await.unwrap();
    assert_eq!(
        r.warnings,
        vec![w(
            1,
            "SELECT v FROM gap_c.t WHERE k = 1 AND c >= 2 LIMIT 100",
            t1
        )]
    );
}
