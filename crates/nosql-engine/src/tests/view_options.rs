//! ScyllaDB's validation of the table options of views and secondary indexes (they are tables
//! too): compaction sub-options, index intervals, memtable flush period. Texts probed on the
//! reference.

use super::*;

#[tokio::test]
async fn view_and_index_options() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.b (p int PRIMARY KEY, v int)"
    ))
    .await;
    let mv = "CREATE MATERIALIZED VIEW ks.vv AS SELECT * FROM ks.b WHERE v IS NOT NULL AND p IS NOT NULL PRIMARY KEY (v, p) WITH";
    let stcs = |t: &str| {
        format!(
            "compaction = {{'class': 'SizeTieredCompactionStrategy', 'tombstone_threshold': '{t}'}}"
        )
    };
    let config = ErrorCode::Config;
    let cases: Vec<(String, ErrorCode, &str)> = vec![
        (format!("{mv} {}", stcs("-1")), config, "tombstone_threshold value (-1) must be between 0.0 and 1.0"),
        (format!("{mv} {}", stcs("abc")), ErrorCode::Syntax, "Invalid double value abc for 'tombstone_threshold'"),
        (format!("CREATE INDEX i ON ks.b (v) WITH {}", stcs("-1")), config, "tombstone_threshold value (-1) must be between 0.0 and 1.0"),
        (format!("CREATE INDEX i ON ks.b (v) WITH {}", stcs("x")), ErrorCode::Syntax, "Invalid double value x for 'tombstone_threshold'"),
        ("CREATE INDEX i ON ks.b (v) WITH min_index_interval = 0".into(), config, "min_index_interval must be greater than 0"),
        ("CREATE INDEX i ON ks.b (v) WITH max_index_interval = 10".into(), config, "max_index_interval must be greater than min_index_interval"),
        ("CREATE INDEX i ON ks.b (v) WITH min_index_interval = 1024 AND max_index_interval = 512".into(), config, "max_index_interval must be greater than min_index_interval"),
        ("CREATE INDEX i ON ks.b (v) WITH memtable_flush_period_in_ms = -1".into(), config, "memtable_flush_period_in_ms must be 0 or greater than 60000"),
        ("CREATE INDEX i ON ks.b (v) WITH memtable_flush_period_in_ms = 100".into(), config, "memtable_flush_period_in_ms must be 0 or greater than 60000"),
        (format!("{mv} min_index_interval = 0"), config, "min_index_interval must be greater than 0"),
        (format!("{mv} memtable_flush_period_in_ms = 100"), config, "memtable_flush_period_in_ms must be 0 or greater than 60000"),
    ];
    for (cql, code, want) in &cases {
        let e = h.err(cql).await;
        assert_eq!((e.code, e.message.as_str()), (*code, *want), "{cql}");
    }
    h.ok(&format!("{mv} {}", stcs("0.5"))).await;
    h.ok("CREATE INDEX i ON ks.b (v) WITH memtable_flush_period_in_ms = 70000")
        .await;
    for (cql, want) in [
        (
            format!("ALTER MATERIALIZED VIEW ks.vv WITH {}", stcs("-1")),
            "tombstone_threshold value (-1) must be between 0.0 and 1.0",
        ),
        (
            format!("ALTER MATERIALIZED VIEW ks.vv WITH {}", stcs("2")),
            "tombstone_threshold value (2) must be between 0.0 and 1.0",
        ),
        (
            "ALTER MATERIALIZED VIEW ks.vv WITH max_index_interval = 10".to_string(),
            "max_index_interval must be greater than min_index_interval",
        ),
    ] {
        let e = h.err(&cql).await;
        assert_eq!((e.code, e.message.as_str()), (config, want), "{cql}");
    }
}
