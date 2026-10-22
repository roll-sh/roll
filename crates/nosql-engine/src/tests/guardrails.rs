//! ScyllaDB's live configuration and guardrails (`crate::guardrails`): `UPDATE system.config`
//! and its failures, the replication, write consistency and large data guardrails, the
//! tombstone page limit, the preparation switches and the counters behind the metrics.
//!
//! Texts marked "reference" were captured on `scylladb/scylla:2026.3.3` (failing writes to
//! `system.config`, the default guardrail thresholds, the large data messages with the image's
//! thresholds). The others need a configuration change the shared references can't take; their
//! texts follow ScyllaDB's patterns (the suites check them by regular expression).

use super::*;
use crate::guardrails::Metrics;
use std::sync::atomic::Ordering;

const SCYLLA_KS: &str = "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}";

async fn scylla() -> H {
    let mut h = H::new(Dialect::Scylla).await;
    h.ok(SCYLLA_KS).await;
    h
}

async fn set(h: &mut H, name: &str, value: &str) {
    h.ok(&format!(
        "UPDATE system.config SET value = '{value}' WHERE name = '{name}'"
    ))
    .await;
}

async fn value(h: &mut H, name: &str) -> (String, String) {
    let r = h
        .rows(&format!(
            "SELECT source, value FROM system.config WHERE name = '{name}'"
        ))
        .await;
    (r[0][0].clone(), r[0][1].clone())
}

fn blob(n: usize) -> Param {
    Param::new(Bytes::from(vec![0u8; n]))
}

fn set_of(range: std::ops::Range<i32>) -> Param {
    let mut b = (range.len() as i32).to_be_bytes().to_vec();
    for i in range {
        b.extend(4i32.to_be_bytes());
        b.extend(i.to_be_bytes());
    }
    Param::new(Bytes::from(b))
}

async fn exec(h: &mut H, cql: &str, values: Vec<Param>) -> Result<Response, Error> {
    let p = h.s.prepare(cql, None).await.unwrap_or_else(|e| panic!("{cql}: {e}"));
    h.s.execute(&p.id, QueryParams::with_values(values)).await
}

fn write_failure(e: &Error) -> (String, String) {
    assert_eq!(e.code, ErrorCode::WriteFailure, "{e}");
    match &e.detail {
        crate::ErrorDetail::WriteFailure {
            write_type,
            consistency,
            received,
            block_for,
            num_failures,
            ..
        } => {
            assert_eq!((*received, *block_for, *num_failures), (0, 1, 1));
            (write_type.clone(), consistency.name().to_string())
        }
        d => panic!("{d:?}"),
    }
}

/// Every refusal of a write to `system.config`, in the reference's order (reference).
#[tokio::test]
async fn config_write_failures() {
    let mut h = scylla().await;
    for (cql, msg, write_type) in [
        ("UPDATE system.config SET value='1' WHERE name='no_such_option'", "no such option", "SIMPLE"),
        ("UPDATE system.config SET value='x' WHERE name='cluster_name'", "option is not live-updateable", "SIMPLE"),
        ("UPDATE system.config SET value='abc' WHERE name='num_tokens'", "option is not live-updateable", "SIMPLE"),
        ("UPDATE system.config SET value='abc' WHERE name='query_tombstone_page_limit'", "cannot parse option value", "SIMPLE"),
        ("UPDATE system.config SET value='maybe' WHERE name='compaction_enforce_min_threshold'", "cannot parse option value", "SIMPLE"),
        ("UPDATE system.config SET source='cql' WHERE name='query_tombstone_page_limit'", "option value is required", "SIMPLE"),
        ("UPDATE system.config SET type='x' WHERE name='query_tombstone_page_limit'", "option value is required", "SIMPLE"),
        ("INSERT INTO system.config (name) VALUES ('query_tombstone_page_limit')", "option value is required", "SIMPLE"),
        ("UPDATE system.config SET value='abc', source='x' WHERE name='query_tombstone_page_limit'", "option source is not updateable", "SIMPLE"),
        ("UPDATE system.config SET value='abc', type='x' WHERE name='query_tombstone_page_limit'", "option type is immutable", "SIMPLE"),
        ("UPDATE system.config SET value='abc', source='x' WHERE name='nosuch'", "option source is not updateable", "SIMPLE"),
        ("UPDATE system.config SET value='x' WHERE name IN ('nosuch1','nosuch2')", "no such option", "UNLOGGED_BATCH"),
        ("INSERT INTO system.config (name, value) VALUES ('nosuch', 'x')", "no such option", "SIMPLE"),
        ("BEGIN BATCH UPDATE system.config SET value='x' WHERE name='nosuch'; APPLY BATCH", "no such option", "SIMPLE"),
        ("UPDATE system.config USING TTL 10 SET value='abc' WHERE name='query_tombstone_page_limit'", "cannot parse option value", "SIMPLE"),
        ("DELETE FROM system.config WHERE name='query_tombstone_page_limit'", "Operation failed for system.config - received 0 responses and 1 failures from 1 CL=ONE.", "SIMPLE"),
        ("DELETE value FROM system.config WHERE name='nosuch'", "Operation failed for system.config - received 0 responses and 1 failures from 1 CL=ONE.", "SIMPLE"),
        ("UPDATE system.config SET value=null WHERE name='query_tombstone_page_limit'", "Operation failed for system.config - received 0 responses and 1 failures from 1 CL=ONE.", "SIMPLE"),
        ("UPDATE system.config SET value='x' WHERE name='nosuch' IF value='y'", "Operation failed for system.config - received 0 responses and 1 failures from 1 CL=ONE.", "CAS"),
        ("INSERT INTO system.config (name, value) VALUES ('nosuch', 'x') IF NOT EXISTS", "Operation failed for system.config - received 0 responses and 1 failures from 1 CL=ONE.", "CAS"),
        ("UPDATE system.runtime_info SET value='x' WHERE group='g' AND item='i'", "this virtual table doesn't allow updates", "SIMPLE"),
    ] {
        let e = h.err(cql).await;
        assert_eq!(e.message, msg, "{cql}");
        assert_eq!(write_failure(&e), (write_type.to_string(), "ONE".to_string()), "{cql}");
    }
    let e = h.err("UPDATE system.config SET value='x' WHERE name=''").await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Invalid, "Key may not be empty"));
    // The request's consistency is the failure's.
    let p = QueryParams {
        consistency: crate::Consistency::Quorum,
        ..Default::default()
    };
    let e = h
        .s
        .query("UPDATE system.config SET value='x' WHERE name='cluster_name'", p)
        .await
        .unwrap_err();
    assert_eq!(write_failure(&e), ("SIMPLE".into(), "QUORUM".into()));
    // Nothing changed.
    assert_eq!(value(&mut h, "query_tombstone_page_limit").await, ("default".into(), "10000".into()));
    // `enable_cql_config_updates = false` closes the table.
    set(&mut h, "enable_cql_config_updates", "false").await;
    let e = h.err("UPDATE system.config SET value='5' WHERE name='query_tombstone_page_limit'").await;
    assert_eq!(e.message, "this virtual table doesn't allow updates");
}

/// Updates apply live, read back as `system.config` shows values (`source = 'cql'`), and the
/// suites' restore of a read value works.
#[tokio::test]
async fn config_updates() {
    let mut h = scylla().await;
    set(&mut h, "query_tombstone_page_limit", "10").await;
    assert_eq!(h.db.settings().scylla.query_tombstone_page_limit, 10);
    assert_eq!(value(&mut h, "query_tombstone_page_limit").await, ("cql".into(), "10".into()));
    // test_system_config_update_boolean: true/false/0/1 round trip as true/false.
    for (v, shown) in [("true", "true"), ("0", "false"), ("1", "true"), ("false", "false")] {
        set(&mut h, "compaction_enforce_min_threshold", v).await;
        assert_eq!(value(&mut h, "compaction_enforce_min_threshold").await.1, shown);
    }
    // Lists: `[a]`, `["a"]`, `a,b`, ``, `[]`; shown as compact JSON.
    for (v, shown) in [
        ("[SimpleStrategy]", "[\"SimpleStrategy\"]"),
        ("[\"SimpleStrategy\"]", "[\"SimpleStrategy\"]"),
        ("SimpleStrategy,EverywhereStrategy", "[\"SimpleStrategy\",\"EverywhereStrategy\"]"),
        ("", "[]"),
        ("[]", "[]"),
    ] {
        set(&mut h, "replication_strategy_warn_list", v).await;
        assert_eq!(value(&mut h, "replication_strategy_warn_list").await.1, shown, "{v}");
    }
    set(&mut h, "write_consistency_levels_warned", "ONE, ALL").await;
    assert_eq!(value(&mut h, "write_consistency_levels_warned").await.1, "[\"ONE\",\"ALL\"]");
    let e = h
        .err("UPDATE system.config SET value='INVALID_CL, ANY' WHERE name='write_consistency_levels_disallowed'")
        .await;
    assert_eq!(e.message, "cannot parse option value");
    // Restriction modes are shown quoted, and their quoted value reads back.
    let (_, shown) = value(&mut h, "restrict_replication_simplestrategy").await;
    assert_eq!(shown, "\"0\"");
    set(&mut h, "restrict_replication_simplestrategy", "warn").await;
    assert_eq!(value(&mut h, "restrict_replication_simplestrategy").await.1, "\"warn\"");
    set(&mut h, "restrict_replication_simplestrategy", &shown).await;
    assert_eq!(h.db.settings().scylla.restrict_replication_simplestrategy, "0");
    // Strings are JSON-quoted; `""` restores an empty one.
    set(&mut h, "vector_store_primary_uri", "http://x:1").await;
    assert_eq!(value(&mut h, "vector_store_primary_uri").await.1, "\"http://x:1\"");
    set(&mut h, "vector_store_primary_uri", "\"\"").await;
    assert_eq!(value(&mut h, "vector_store_primary_uri").await.1, "\"\"");
    // Typed settings behind the table.
    set(&mut h, "enable_create_table_with_compact_storage", "true").await;
    assert!(h.db.settings().compact_storage_enabled);
    set(&mut h, "batch_size_warn_threshold_in_kb", "7").await;
    assert_eq!(h.db.settings().batch_size_warn_threshold, 7 * 1024);
    set(&mut h, "strict_allow_filtering", "true").await;
    assert_eq!(h.db.settings().strict_allow_filtering, crate::StrictAllowFiltering::True);
    assert_eq!(value(&mut h, "strict_allow_filtering").await, ("cql".into(), "\"true\"".into()));
    set(&mut h, "minimum_replication_factor_warn_threshold", "-1").await;
    assert_eq!(h.db.settings().minimum_replication_factor_warn_threshold, -1);
}

/// Cassandra 5's settings table takes no writes.
#[tokio::test]
async fn cassandra_settings_are_read_only() {
    let mut h = H::new(Dialect::Cassandra).await;
    let e = h
        .err("UPDATE system_views.settings SET value='1' WHERE name='tombstone_warn_threshold'")
        .await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::Invalid, "Modification is not supported by table system_views.settings")
    );
}

/// `replication_strategy_{warn,fail}_list`, `restrict_replication_simplestrategy` and the four
/// RF thresholds, on CREATE and ALTER, with their counters.
#[tokio::test]
async fn replication_guardrails() {
    let mut h = scylla().await;
    let m = |h: &H| -> [u64; 6] {
        let m: &Metrics = h.db.guardrail_metrics();
        [
            m.replication_strategy_warn_list_violations.load(Ordering::Relaxed),
            m.replication_strategy_fail_list_violations.load(Ordering::Relaxed),
            m.minimum_replication_factor_warn_violations.load(Ordering::Relaxed),
            m.minimum_replication_factor_fail_violations.load(Ordering::Relaxed),
            m.maximum_replication_factor_warn_violations.load(Ordering::Relaxed),
            m.maximum_replication_factor_fail_violations.load(Ordering::Relaxed),
        ]
    };
    let simple = |rf: u32| {
        format!("WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': {rf}}} AND tablets = {{'enabled': false}}")
    };
    // Defaults (reference): SimpleStrategy warns, RF 1 < 3 warns.
    let r = h.ok(&format!("CREATE KEYSPACE k1 {}", simple(1))).await;
    assert_eq!(r.warnings, [
        "SimpleStrategy replication class is not recommended, but was used for keyspace k1. You may suppress this warning by delisting SimpleStrategy from replication_strategy_warn_list configuration option, or make it into an error by listing this replication strategy on replication_strategy_fail_list.",
        "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended.",
    ]);
    // (`ks` itself warned once.)
    assert_eq!(m(&h), [1, 0, 2, 0, 0, 0]);
    // EverywhereStrategy / LocalStrategy keep any options; numeric ones are RF-checked
    // (reference).
    let r = h
        .ok("CREATE KEYSPACE k2 WITH replication = {'class': 'EverywhereStrategy', 'foo': 1} AND tablets = {'enabled': false}")
        .await;
    assert_eq!(r.warnings, ["Using Replication Factor foo=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended."]);
    h.ok("CREATE KEYSPACE k3 WITH replication = {'class': 'LocalStrategy', 'replication_factor': 'x'} AND tablets = {'enabled': false}").await;
    assert_eq!(
        h.rows("SELECT replication FROM system_schema.keyspaces WHERE keyspace_name = 'k3'").await,
        table(&[&["{'class': 'org.apache.cassandra.locator.LocalStrategy', 'replication_factor': 'x'}"]])
    );
    // The fail list wins over the warn list, without warnings.
    set(&mut h, "replication_strategy_fail_list", "SimpleStrategy,EverywhereStrategy").await;
    let e = h.err(&format!("CREATE KEYSPACE k4 {}", simple(3))).await;
    assert_eq!(e.code, ErrorCode::Config);
    assert_eq!(e.message, "SimpleStrategy replication class is not recommended, and forbidden by the current configuration, but was used for keyspace k4. You may override this restriction by modifying replication_strategy_fail_list configuration option to not list SimpleStrategy.");
    assert_eq!(m(&h)[1], 1);
    // ALTER is checked too.
    h.ok("CREATE KEYSPACE k5 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3} AND tablets = {'enabled': false}").await;
    let e = h.err("ALTER KEYSPACE k5 WITH replication = {'class': 'EverywhereStrategy', 'replication_factor': 3}").await;
    assert!(e.message.starts_with("EverywhereStrategy replication class is not recommended, and forbidden"), "{}", e.message);
    set(&mut h, "replication_strategy_fail_list", "[]").await;
    set(&mut h, "replication_strategy_warn_list", "").await;
    assert!(h.ok(&format!("CREATE KEYSPACE k6 {}", simple(3))).await.warnings.is_empty());
    // The legacy switch.
    set(&mut h, "restrict_replication_simplestrategy", "warn").await;
    assert_eq!(h.ok(&format!("CREATE KEYSPACE k7 {}", simple(3))).await.warnings.len(), 1);
    set(&mut h, "restrict_replication_simplestrategy", "true").await;
    assert!(h.err(&format!("CREATE KEYSPACE k8 {}", simple(3))).await.message.contains("forbidden"));
    set(&mut h, "restrict_replication_simplestrategy", "false").await;
    // RF thresholds; RF 0 never trips them, -1 turns a threshold off.
    set(&mut h, "minimum_replication_factor_fail_threshold", "2").await;
    set(&mut h, "maximum_replication_factor_warn_threshold", "5").await;
    set(&mut h, "maximum_replication_factor_fail_threshold", "7").await;
    let nts = |rf: u32| format!("WITH replication = {{'class': 'NetworkTopologyStrategy', 'datacenter1': {rf}}} AND tablets = {{'enabled': false}}");
    let e = h.err(&format!("CREATE KEYSPACE k9 {}", nts(1))).await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Config, "Replication Factor datacenter1=1 is forbidden by the current configuration setting of minimum_replication_factor_fail_threshold=2. Please increase replication factor, or lower minimum_replication_factor_fail_threshold set in the configuration."));
    let r = h.ok(&format!("CREATE KEYSPACE k10 {}", nts(6))).await;
    assert_eq!(r.warnings, ["Using Replication Factor datacenter1=6 greater than the maximum_replication_factor_warn_threshold=5 is not recommended."]);
    let e = h.err(&format!("CREATE KEYSPACE k11 {}", nts(8))).await;
    assert_eq!(e.message, "Replication Factor datacenter1=8 is forbidden by the current configuration setting of maximum_replication_factor_fail_threshold=7. Please decrease replication factor, or increase maximum_replication_factor_fail_threshold set in the configuration.");
    let r = h.ok("CREATE KEYSPACE k12 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 0}").await;
    assert!(r.warnings.is_empty());
    let e = h.err("ALTER KEYSPACE k5 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}").await;
    assert!(e.message.contains("minimum_replication_factor_fail_threshold=2"));
    // (`restrict_replication_simplestrategy = true` counts as a fail list violation.)
    assert_eq!(m(&h), [2, 3, 3, 2, 1, 1]);
    for n in ["minimum_replication_factor_warn_threshold", "minimum_replication_factor_fail_threshold", "maximum_replication_factor_warn_threshold", "maximum_replication_factor_fail_threshold"] {
        set(&mut h, n, "-1").await;
    }
    assert!(h.ok(&format!("CREATE KEYSPACE k13 {}", nts(100))).await.warnings.is_empty());
    assert!(h.ok(&format!("CREATE KEYSPACE k14 {}", nts(1))).await.warnings.is_empty());
}

/// `write_consistency_levels_{warned,disallowed}` for every write kind; reads aren't
/// affected; writes are counted per level.
#[tokio::test]
async fn write_consistency_guardrails() {
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (pk int PRIMARY KEY, v int);\nCREATE TABLE ks.c (pk int PRIMARY KEY, c counter)").await;
    let at = |cl: crate::Consistency| QueryParams {
        consistency: cl,
        ..Default::default()
    };
    let writes = [
        "INSERT INTO ks.t (pk, v) VALUES (1, 1)",
        "UPDATE ks.t SET v = 2 WHERE pk = 1",
        "DELETE FROM ks.t WHERE pk = 1",
        "INSERT INTO ks.t (pk, v) VALUES (5, 5) IF NOT EXISTS",
        "UPDATE ks.c SET c = c + 1 WHERE pk = 1",
        "BEGIN BATCH INSERT INTO ks.t (pk, v) VALUES (2, 2); UPDATE ks.t SET v = 3 WHERE pk = 3; APPLY BATCH",
        "BEGIN UNLOGGED BATCH INSERT INTO ks.t (pk, v) VALUES (4, 4); APPLY BATCH",
        "BEGIN BATCH INSERT INTO ks.t (pk, v) VALUES (6, 6) IF NOT EXISTS; APPLY BATCH",
    ];
    set(&mut h, "write_consistency_levels_warned", "ONE").await;
    for w in writes {
        let before = h.db.guardrail_metrics().write_consistency_levels_warned_violations.load(Ordering::Relaxed);
        let r = h.s.query(w, at(crate::Consistency::One)).await.unwrap_or_else(|e| panic!("{w}: {e}"));
        let after = h.db.guardrail_metrics().write_consistency_levels_warned_violations.load(Ordering::Relaxed);
        assert_eq!(after, before + 1, "{w}");
        assert_eq!(r.warnings, [crate::guardrails::write_cl::warned_message(crate::Consistency::One)], "{w}");
    }
    set(&mut h, "write_consistency_levels_warned", "").await;
    set(&mut h, "write_consistency_levels_disallowed", "ONE").await;
    for w in writes {
        let e = h.s.query(w, at(crate::Consistency::One)).await.unwrap_err();
        assert_eq!((e.code, e.message), (ErrorCode::Invalid, crate::guardrails::write_cl::disallowed_message(crate::Consistency::One)), "{w}");
    }
    assert!(crate::guardrails::write_cl::disallowed_message(crate::Consistency::One).starts_with("Write consistency level ONE is forbidden"));
    // Reads and other levels pass silently.
    let r = h.s.query("SELECT * FROM ks.t WHERE pk = 1", at(crate::Consistency::One)).await.unwrap();
    assert!(r.warnings.is_empty());
    let r = h.s.query(writes[0], at(crate::Consistency::All)).await.unwrap();
    assert!(r.warnings.is_empty());
    let m = h.db.guardrail_metrics();
    // `UPDATE system.config` is a write too: clearing the warned list was warned about.
    assert_eq!(m.write_consistency_levels_warned_violations.load(Ordering::Relaxed), 9);
    assert_eq!(m.write_consistency_levels_disallowed_violations.load(Ordering::Relaxed), 8);
    // 8 + 8 writes and the 4 configuration updates (DDL isn't a write).
    assert_eq!(m.writes_per_consistency_level[crate::Consistency::One.code() as usize].load(Ordering::Relaxed), 20);
    assert_eq!(m.writes_per_consistency_level[crate::Consistency::All.code() as usize].load(Ordering::Relaxed), 1);
    let mut out = String::new();
    m.render(&mut out);
    assert!(out.contains("scylla_cql_writes_per_consistency_level{consistency_level=\"ONE\",shard=\"0\"} 20\n"));
    // Cassandra has none of this.
    let mut c = H::new(Dialect::Cassandra).await;
    c.exec(&format!("{KS};\nCREATE TABLE ks.t (pk int PRIMARY KEY, v int)")).await;
    c.ok("INSERT INTO ks.t (pk, v) VALUES (1, 1)").await;
    assert_eq!(c.db.guardrail_metrics().writes_per_consistency_level[1].load(Ordering::Relaxed), 0);
}

async fn shown(h: &mut H) -> Vec<Vec<String>> {
    h.rows("SELECT table_name, large_data_guardrails_enabled FROM system_schema.scylla_tables WHERE keyspace_name = 'ks'")
        .await
}

/// `large_data_guardrails_enabled`: values, storage in `scylla_tables`, persistence, not in
/// DESCRIBE (reference).
#[tokio::test]
async fn large_data_table_option() {
    let mut h = scylla().await;
    h.ok("CREATE TABLE ks.t (pk int PRIMARY KEY, v int) WITH large_data_guardrails_enabled = false").await;
    assert_eq!(shown(&mut h).await, table(&[&["t", "null"]]));
    h.ok("ALTER TABLE ks.t WITH large_data_guardrails_enabled = 'yes'").await;
    assert_eq!(shown(&mut h).await, table(&[&["t", "True"]]));
    h.ok("ALTER TABLE ks.t WITH comment = 'x'").await;
    assert_eq!(shown(&mut h).await, table(&[&["t", "True"]]));
    let d = h.rows("DESC TABLE ks.t").await;
    assert!(!d[0][3].contains("large_data"), "{}", d[0][3]);
    h.ok("ALTER TABLE ks.t WITH large_data_guardrails_enabled = 'no'").await;
    assert_eq!(shown(&mut h).await, table(&[&["t", "null"]]));
    h.ok("ALTER TABLE ks.t WITH large_data_guardrails_enabled = 1").await;
    let e = h.err("ALTER TABLE ks.t WITH large_data_guardrails_enabled = {'a': 1}").await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (ErrorCode::Syntax, "Invalid value for property 'large_data_guardrails_enabled'. It should be a string")
    );
    // Reopened from storage.
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", Dialect::Scylla).await;
    assert_eq!(shown(&mut h).await, table(&[&["t", "True"]]));
    assert!(h.db.schema().keyspace("ks").unwrap().tables["t"].params.large_data_guardrails);
    // Cassandra doesn't know it.
    let mut c = H::new(Dialect::Cassandra).await;
    c.exec(KS).await;
    let e = c.err("CREATE TABLE ks.t (pk int PRIMARY KEY) WITH large_data_guardrails_enabled = true").await;
    assert_eq!(e.message, "Unknown property 'large_data_guardrails_enabled'");
}

/// The coordinator checks: cells, collections, rows; soft warnings; LWT; batches; deletes and
/// tables without the option pass (reference texts with the image's thresholds).
#[tokio::test]
async fn large_data_coordinator() {
    const MB: usize = 1024 * 1024;
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (pk int, ck int, s blob STATIC, v blob, v2 blob, l list<int>, st set<int>, PRIMARY KEY (pk, ck)) WITH large_data_guardrails_enabled = true;\n\
            CREATE TABLE ks.off (pk int PRIMARY KEY, v blob)").await;
    let e = exec(&mut h, "INSERT INTO ks.t (pk, ck, v) VALUES (1, 1, ?)", vec![blob(2 * MB + 1)]).await.unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Invalid, "Large data guardrail: cell value size 2097153 exceeds hard limit 2097152 (keyspace=ks, table=t, column=v)"));
    let r = exec(&mut h, "INSERT INTO ks.t (pk, ck, v) VALUES (1, 1, ?)", vec![blob(2 * MB)]).await.unwrap();
    assert_eq!(r.warnings, ["Large data guardrail: Soft limit violation for cell"]);
    let e = exec(&mut h, "INSERT INTO ks.t (pk, s) VALUES (1, ?)", vec![blob(2 * MB + 1)]).await.unwrap_err();
    assert_eq!(e.message, "Large data guardrail: cell value size 2097153 exceeds hard limit 2097152 (keyspace=ks, table=t, column=s)");
    for q in [
        "UPDATE ks.t SET v = ? WHERE pk = 1 AND ck = 3",
        "INSERT INTO ks.t (pk, ck, v) VALUES (1, 7, ?) IF NOT EXISTS",
        "BEGIN BATCH INSERT INTO ks.t (pk, ck, v) VALUES (1, 8, ?) APPLY BATCH",
    ] {
        let e = exec(&mut h, q, vec![blob(2 * MB + 1)]).await.unwrap_err();
        assert!(e.message.starts_with("Large data guardrail: cell value size 2097153"), "{q}: {e}");
    }
    let e = exec(&mut h, "INSERT INTO ks.t (pk, ck, st) VALUES (1, 6, ?)", vec![set_of(0..20001)]).await.unwrap_err();
    assert_eq!(e.message, "Large data guardrail: collection element count 20001 exceeds hard limit 20000 (keyspace=ks, table=t, column=st)");
    let r = exec(&mut h, "INSERT INTO ks.t (pk, ck, st) VALUES (1, 6, ?)", vec![set_of(0..10001)]).await.unwrap();
    assert_eq!(r.warnings, ["Large data guardrail: Soft limit violation for collection"]);
    exec(&mut h, "INSERT INTO ks.off (pk, v) VALUES (1, ?)", vec![blob(2 * MB + 1)]).await.unwrap();
    // Row size (thresholds lowered).
    set(&mut h, "large_row_fail_threshold_mb", "1").await;
    let e = exec(&mut h, "INSERT INTO ks.t (pk, ck, v, v2) VALUES (2, 3, ?, ?)", vec![blob(MB / 2 + 10), blob(MB / 2 + 10)]).await.unwrap_err();
    assert!(e.message.starts_with("Large data guardrail: row size ") && e.message.ends_with("exceeds hard limit 1048576 (keyspace=ks, table=t, clustering_key=3)"), "{}", e.message);
    let e = exec(&mut h, "INSERT INTO ks.t (pk, s) VALUES (2, ?)", vec![blob(MB + 1)]).await.unwrap_err();
    assert!(e.message.contains("static row size") && e.message.ends_with("(keyspace=ks, table=t)"), "{}", e.message);
    set(&mut h, "large_row_fail_threshold_mb", "0").await;
    set(&mut h, "compaction_large_row_warning_threshold_mb", "1").await;
    let r = exec(&mut h, "INSERT INTO ks.t (pk, ck, v) VALUES (2, 4, ?)", vec![blob(MB + 1)]).await.unwrap();
    assert_eq!(r.warnings, ["Large data guardrail: Soft limit violation for row, cell"]);
    // Soft limits without CQL warnings.
    set(&mut h, "large_data_cql_warnings", "false").await;
    assert!(exec(&mut h, "INSERT INTO ks.t (pk, ck, v) VALUES (2, 5, ?)", vec![blob(MB + 1)]).await.unwrap().warnings.is_empty());
    // A hard limit of 0 is off; deletes always pass.
    set(&mut h, "large_cell_fail_threshold_mb", "0").await;
    exec(&mut h, "INSERT INTO ks.t (pk, ck, v) VALUES (3, 1, ?)", vec![blob(3 * MB)]).await.unwrap();
    set(&mut h, "large_cell_fail_threshold_mb", "1").await;
    h.ok("DELETE FROM ks.t WHERE pk = 3 AND ck = 1").await;
    h.ok("BEGIN BATCH DELETE FROM ks.t WHERE pk = 3 AND ck = 1; APPLY BATCH").await;
    let m = h.db.guardrail_metrics();
    assert!(m.large_cell_exceeding_threshold.load(Ordering::Relaxed) >= 3);
    assert!(m.large_collection_exceeding_threshold.load(Ordering::Relaxed) >= 1);
}

/// The replica checks against stored data: partitions as flushed, rows and collections as
/// merged; LWT and `DELETE … BYPASS LARGE_DATA_GUARDRAILS` skip them (reference texts).
#[tokio::test]
async fn large_data_replica() {
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (pk int, ck int, c2 text, s set<int>, v1 blob, v2 blob, PRIMARY KEY (pk, ck, c2)) WITH large_data_guardrails_enabled = true").await;
    // Collections: the third write finds 60 stored elements.
    set(&mut h, "large_collection_elements_fail_threshold", "50").await;
    set(&mut h, "compaction_collection_elements_count_warning_threshold", "40").await;
    let up = "UPDATE ks.t SET s = s + ? WHERE pk = 1 AND ck = 0 AND c2 = 'x'";
    exec(&mut h, up, vec![set_of(0..30)]).await.unwrap();
    exec(&mut h, up, vec![set_of(30..60)]).await.unwrap();
    let e = exec(&mut h, up, vec![set_of(99..100)]).await.unwrap_err();
    assert_eq!(e.message, "Write rejected for ks.t: Large data guardrail: collection element count 60 exceeds hard limit 50 (keyspace=ks, table=t, column=s, clustering_key=0:x)");
    assert_eq!(write_failure(&e), ("SIMPLE".into(), "ONE".into()));
    // Writes that leave the collection alone pass; other rows and partitions too.
    h.ok("INSERT INTO ks.t (pk, ck, c2, v1) VALUES (1, 0, 'x', 0x00)").await;
    exec(&mut h, up.replace("ck = 0", "ck = 9").as_str(), vec![set_of(0..1)]).await.unwrap();
    // LWT skips the replica checks.
    h.ok("UPDATE ks.t SET s = s + {100} WHERE pk = 1 AND ck = 0 AND c2 = 'x' IF EXISTS").await;
    // Rows.
    set(&mut h, "large_row_fail_threshold_mb", "1").await;
    exec(&mut h, "INSERT INTO ks.t (pk, ck, c2, v1) VALUES (2, 0, 'y', ?)", vec![blob(600 * 1024)]).await.unwrap();
    exec(&mut h, "INSERT INTO ks.t (pk, ck, c2, v2) VALUES (2, 0, 'y', ?)", vec![blob(600 * 1024)]).await.unwrap();
    let e = h.err("INSERT INTO ks.t (pk, ck, c2, v1) VALUES (2, 0, 'y', 0x00)").await;
    assert!(e.message.starts_with("Write rejected for ks.t: Large data guardrail: row size ") && e.message.ends_with(" exceeds hard limit 1048576 (keyspace=ks, table=t, clustering_key=0:y)"), "{}", e.message);
    let e = h.err("DELETE FROM ks.t WHERE pk = 2 AND ck = 0 AND c2 = 'y'").await;
    assert_eq!(e.code, ErrorCode::WriteFailure);
    h.ok("DELETE FROM ks.t WHERE pk = 2 AND ck = 0 AND c2 = 'y' BYPASS LARGE_DATA_GUARDRAILS").await;
    h.ok("INSERT INTO ks.t (pk, ck, c2, v1) VALUES (2, 1, 'y', 0x00)").await;
    // Partitions: only what is flushed counts.
    set(&mut h, "rows_count_fail_threshold", "50").await;
    for ck in 0..60 {
        h.ok(&format!("INSERT INTO ks.t (pk, ck, c2) VALUES (3, {ck}, 'z')")).await;
    }
    h.db.flush().await.unwrap();
    let e = h.err("INSERT INTO ks.t (pk, ck, c2) VALUES (3, 99, 'z')").await;
    assert_eq!(e.message, "Write rejected for ks.t: Large data guardrail: partition row count 60 exceeds hard limit 50 (keyspace=ks, table=t, partition_key=pk{000400000003})");
    h.ok("INSERT INTO ks.t (pk, ck, c2) VALUES (4, 0, 'z')").await;
    set(&mut h, "rows_count_fail_threshold", "0").await;
    set(&mut h, "large_partition_fail_threshold_mb", "1").await;
    exec(&mut h, "INSERT INTO ks.t (pk, ck, c2, v1) VALUES (5, 0, 'a', ?)", vec![blob(600 * 1024)]).await.unwrap();
    exec(&mut h, "INSERT INTO ks.t (pk, ck, c2, v1) VALUES (5, 1, 'a', ?)", vec![blob(600 * 1024)]).await.unwrap();
    // Not flushed yet: accepted.
    h.ok("INSERT INTO ks.t (pk, ck, c2) VALUES (5, 2, 'a')").await;
    h.db.flush().await.unwrap();
    let e = h.err("INSERT INTO ks.t (pk, ck, c2) VALUES (5, 3, 'a')").await;
    assert!(e.message.starts_with("Write rejected for ks.t: Large data guardrail: partition size ") && e.message.ends_with("exceeds hard limit 1048576 (keyspace=ks, table=t, partition_key=pk{000400000005})"), "{}", e.message);
    // The soft partition limit warns.
    set(&mut h, "large_partition_fail_threshold_mb", "0").await;
    set(&mut h, "compaction_large_partition_warning_threshold_mb", "1").await;
    let r = h.ok("INSERT INTO ks.t (pk, ck, c2) VALUES (5, 3, 'a')").await;
    assert_eq!(r.warnings, ["Large data guardrail: Soft limit violation for partition"]);
    // Disabling the option on the table stops everything.
    h.ok("ALTER TABLE ks.t WITH large_data_guardrails_enabled = false").await;
    exec(&mut h, up, vec![set_of(200..201)]).await.unwrap();
}

/// `BYPASS LARGE_DATA_GUARDRAILS` is only DELETE syntax (reference).
#[tokio::test]
async fn bypass_syntax() {
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (pk int PRIMARY KEY, v blob) WITH large_data_guardrails_enabled = true").await;
    for (q, ok) in [
        ("DELETE FROM ks.t WHERE pk = 1 BYPASS LARGE_DATA_GUARDRAILS", true),
        ("DELETE FROM ks.t USING TIMESTAMP 12345 WHERE pk = 1 BYPASS LARGE_DATA_GUARDRAILS", true),
        ("DELETE FROM ks.t WHERE pk = 1 IF EXISTS BYPASS LARGE_DATA_GUARDRAILS", true),
        ("BEGIN BATCH DELETE FROM ks.t WHERE pk = 1 BYPASS LARGE_DATA_GUARDRAILS; APPLY BATCH", true),
        ("INSERT INTO ks.t (pk, v) VALUES (1, 0x00) BYPASS LARGE_DATA_GUARDRAILS", false),
        ("UPDATE ks.t SET v = 0x00 WHERE pk = 1 BYPASS LARGE_DATA_GUARDRAILS", false),
        ("BEGIN BATCH INSERT INTO ks.t (pk, v) VALUES (1, 0x00) APPLY BATCH BYPASS LARGE_DATA_GUARDRAILS", false),
        ("DELETE FROM ks.t WHERE pk = 1 BYPASS LARGE DATA GUARDRAILS", false),
    ] {
        let r = h.run(q).await;
        if ok {
            r.unwrap_or_else(|e| panic!("{q}: {e}"));
        } else {
            assert_eq!(r.unwrap_err().code, ErrorCode::Syntax, "{q}");
        }
    }
}

/// `query_tombstone_page_limit`: pages end after that many tombstones, even empty; unpaged
/// reads aren't cut.
#[tokio::test]
async fn tombstone_page_limit() {
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck));\n\
            CREATE TABLE ks.p (pk int, ck int, v int, s int static, PRIMARY KEY (pk, ck))").await;
    set(&mut h, "query_tombstone_page_limit", "10").await;
    async fn pages(h: &mut H, q: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        let mut state = None;
        loop {
            let p = QueryParams {
                page_size: Some(10),
                paging_state: state.take(),
                ..Default::default()
            };
            let (rows, next) = h.rows_with(q, p).await;
            out.push(rows.into_iter().map(|r| r[1].clone()).collect());
            match next {
                Some(n) => state = Some(n),
                None => return out,
            }
        }
    }
    let strs = |v: &[&[i32]]| -> Vec<Vec<String>> {
        v.iter().map(|p| p.iter().map(|i| i.to_string()).collect()).collect()
    };
    // Row tombstones: a prefix, then a span between live rows.
    for ck in 0..30 {
        h.ok(&format!("DELETE FROM ks.t WHERE pk = 1 AND ck = {ck}")).await;
    }
    h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (1, 31, 0)").await;
    for q in ["SELECT * FROM ks.t WHERE pk = 1", "SELECT * FROM ks.t WHERE pk = 1 AND v = 0 ALLOW FILTERING"] {
        assert_eq!(pages(&mut h, q).await, strs(&[&[], &[], &[], &[31]]), "{q}");
    }
    for ck in 0..11 {
        h.ok(&format!("INSERT INTO ks.t (pk, ck, v) VALUES (2, {ck}, 0)")).await;
    }
    for ck in 30..50 {
        h.ok(&format!("DELETE FROM ks.t WHERE pk = 2 AND ck = {ck}")).await;
    }
    h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (2, 51, 0)").await;
    assert_eq!(pages(&mut h, "SELECT * FROM ks.t WHERE pk = 2").await, strs(&[&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], &[10], &[], &[51]]));
    // Range tombstones: two bounds each.
    for ck in (0..48).step_by(4) {
        h.ok(&format!("DELETE FROM ks.t WHERE pk = 3 AND ck > {ck} AND ck < {}", ck + 3)).await;
    }
    h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (3, 2000, 0)").await;
    assert_eq!(pages(&mut h, "SELECT * FROM ks.t WHERE pk = 3").await, strs(&[&[], &[], &[2000]]));
    // A page ending at a bound that excludes its value resumes right there.
    for ck in 0..12 {
        h.ok(&format!("INSERT INTO ks.t (pk, ck, v) VALUES (5, {ck}, 0)")).await;
    }
    for ck in (32..72).step_by(4) {
        h.ok(&format!("DELETE FROM ks.t WHERE pk = 5 AND ck > {ck} AND ck < {}", ck + 3)).await;
    }
    h.ok("INSERT INTO ks.t (pk, ck, v) VALUES (5, 2000, 0)").await;
    assert_eq!(pages(&mut h, "SELECT * FROM ks.t WHERE pk = 5").await, strs(&[&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], &[10, 11], &[], &[2000]]));
    // Reversed: the same cuts, mirrored.
    assert_eq!(
        pages(&mut h, "SELECT * FROM ks.t WHERE pk = 5 ORDER BY ck DESC").await.concat(),
        ["2000", "11", "10", "9", "8", "7", "6", "5", "4", "3", "2", "1", "0"]
    );
    // Empty rows (a dead cell, no row marker).
    for ck in 0..20 {
        h.ok(&format!("UPDATE ks.t SET v = 0 WHERE pk = 4 AND ck = {ck}")).await;
    }
    for ck in 0..16 {
        h.ok(&format!("DELETE v FROM ks.t WHERE pk = 4 AND ck = {ck}")).await;
    }
    assert_eq!(pages(&mut h, "SELECT * FROM ks.t WHERE pk = 4").await, strs(&[&[], &[16, 17, 18, 19]]));
    // Unpaged: everything at once.
    assert_eq!(h.rows("SELECT * FROM ks.t WHERE pk = 4").await.len(), 4);
    // Partition tombstones in a scan: at least one empty page before the last live partition.
    for pk in 0..40 {
        h.ok(&format!("INSERT INTO ks.p (pk, ck, v) VALUES ({pk}, 0, 0)")).await;
    }
    for pk in 0..39 {
        h.ok(&format!("DELETE FROM ks.p WHERE pk = {pk}")).await;
    }
    let got = pages(&mut h, "SELECT * FROM ks.p").await;
    assert!(got.len() >= 2 && got.iter().filter(|p| !p.is_empty()).count() == 1, "{got:?}");
    // The limit is off for Cassandra (and when 0).
    set(&mut h, "query_tombstone_page_limit", "0").await;
    assert_eq!(pages(&mut h, "SELECT * FROM ks.t WHERE pk = 1").await, strs(&[&[31]]));
}

/// `cql_duplicate_bind_variable_names_refer_to_same_variable` and
/// `cql_in_bind_variable_name_uses_uppercase_operator`.
#[tokio::test]
async fn prepare_switches() {
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (p int, c int, PRIMARY KEY (p, c));\nCREATE TABLE ks.u (p int PRIMARY KEY, v int)").await;
    let names = |p: &crate::response::Prepared| -> Vec<String> {
        p.bind.iter().map(|c| c.name.clone()).collect()
    };
    let p = h.s.prepare("SELECT * FROM ks.t WHERE p = :x AND c = :x", None).await.unwrap();
    assert_eq!((names(&p), p.pk_indexes.clone()), (vec!["x".to_string()], vec![0]));
    let p = h.s.prepare("SELECT * FROM ks.t WHERE p = ? AND c IN ?", None).await.unwrap();
    assert_eq!(names(&p), ["p", "IN(c)"]);
    set(&mut h, "cql_duplicate_bind_variable_names_refer_to_same_variable", "false").await;
    set(&mut h, "cql_in_bind_variable_name_uses_uppercase_operator", "false").await;
    let p = h.s.prepare("SELECT * FROM ks.t WHERE p = :x AND c = :x", None).await.unwrap();
    assert_eq!((names(&p), p.pk_indexes.clone()), (vec!["x".to_string(), "x".to_string()], vec![0]));
    let p = h.s.prepare("SELECT * FROM ks.t WHERE c = :x AND p = :x", None).await.unwrap();
    assert_eq!(p.pk_indexes, [1]);
    for (q, want) in [
        ("SELECT * FROM ks.t WHERE p = ? AND c IN ?", ["p", "in(c)"]),
        ("SELECT * FROM ks.t WHERE p = ? AND c NOT IN ? ALLOW FILTERING", ["p", "not in(c)"]),
        ("UPDATE ks.u SET v = 1 WHERE p = ? IF v IN ?", ["p", "in(v)"]),
    ] {
        let p = h.s.prepare(q, None).await.unwrap();
        assert_eq!(names(&p), want, "{q}");
    }
}

/// `enable_cassio_compatibility`: CassIO's SAI `ENTRIES` index on a map becomes a regular
/// secondary index.
#[tokio::test]
async fn cassio_compatibility() {
    let mut h = scylla().await;
    h.exec("CREATE TABLE ks.t (p int PRIMARY KEY, m map<text, text>)").await;
    let sai = "CREATE CUSTOM INDEX i ON ks.t (ENTRIES(m)) USING 'org.apache.cassandra.index.sai.StorageAttachedIndex'";
    assert!(h.msg(sai).await.contains("enable_cassio_compatibility"));
    set(&mut h, "enable_cassio_compatibility", "true").await;
    for opts in ["{'some_option': 'value'}", "{}"] {
        let e = h.err(&format!("CREATE CUSTOM INDEX ON ks.t (ENTRIES(m)) USING 'sai' WITH OPTIONS = {opts}")).await;
        assert!(e.message.contains("cannot be rewritten") && e.message.contains("WITH OPTIONS"), "{}", e.message);
    }
    let r = h.ok(sai).await;
    assert!(r.warnings.iter().any(|w| w.contains("SAI") && w.contains("regular secondary index")), "{:?}", r.warnings);
    assert_eq!(
        h.rows("SELECT kind, options FROM system_schema.indexes WHERE keyspace_name = 'ks' AND table_name = 't' AND index_name = 'i'").await,
        table(&[&["COMPOSITES", "{'target': 'entries(m)'}"]])
    );
}

/// ScyllaDB lets `memtable_flush_period_in_ms` alone be set on a system table.
#[tokio::test]
async fn system_table_flush_period() {
    let mut h = scylla().await;
    h.ok("ALTER TABLE system.compaction_history WITH memtable_flush_period_in_ms = 80000").await;
    for q in [
        "ALTER TABLE system.compaction_history WITH comment = ''",
        "ALTER TABLE system.compaction_history WITH memtable_flush_period_in_ms = 80000 AND comment = ''",
    ] {
        assert_eq!(h.err(q).await.code, ErrorCode::Unauthorized, "{q}");
    }
}
