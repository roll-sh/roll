//! ScyllaDB keyspace and table options probed on `scylladb/scylla:2026.3.3`: rack-list
//! replication (`'dc': ['rack1']`, `replication_v2`), the default datacenter RF, ALTER rules,
//! `STORAGE`, keyspace `tablets` on ALTER, the per-table `tablets` hints, name lengths.

use super::*;

fn config(e: Error) -> String {
    assert_eq!(e.code, ErrorCode::Config, "{e}");
    e.message
}

async fn scylla() -> H {
    H::new(Dialect::Scylla).await
}

async fn replication(h: &mut H, ks: &str) -> Vec<Vec<String>> {
    h.rows(&format!(
        "SELECT replication, replication_v2 FROM system_schema.keyspaces WHERE keyspace_name = '{ks}'"
    ))
    .await
}

async fn desc(h: &mut H, what: &str) -> String {
    h.rows(&format!("DESC {what}")).await[0][3].clone()
}

#[tokio::test]
async fn rack_lists() {
    let mut h = scylla().await;
    let nts = "'class': 'NetworkTopologyStrategy'";
    let r = h.ok(&format!("CREATE KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': ['rack1']}}")).await;
    assert_eq!(r.warnings, ["Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended."]);
    let lists = table(&[&[
        "{'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'}",
        "{'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1:0': 'rack1'}",
    ]]);
    assert_eq!(replication(&mut h, "r1").await, lists);
    assert_eq!(
        desc(&mut h, "KEYSPACE r1").await,
        "CREATE KEYSPACE r1 WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': ['rack1']} AND durable_writes = true AND tablets = {'enabled': true};"
    );
    // The same count keeps the list; 0 or [] drop the datacenter; another count fails.
    h.ok(&format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': 1}}")).await;
    assert_eq!(replication(&mut h, "r1").await, lists);
    let e = h.err(&format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': 2}}")).await;
    assert_eq!(config(e), "Cannot change replication factor for 'datacenter1' from ['rack1'] to '2' when the old value was a rack list");
    for gone in ["[]", "0"] {
        h.ok(&format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': ['rack1']}}")).await;
        h.ok(&format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': {gone}}}")).await;
        assert_eq!(
            replication(&mut h, "r1").await,
            table(&[&["{'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy'}", "null"]])
        );
    }
    h.ok(&format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': ['rack1']}}")).await;
    assert_eq!(replication(&mut h, "r1").await, lists);
    // Kept across a reopen.
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", Dialect::Scylla).await;
    assert_eq!(replication(&mut h, "r1").await, lists);
    assert!(desc(&mut h, "KEYSPACE r1").await.contains("'datacenter1': ['rack1']"));
    for (cql, msg) in [
        (format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': ['rack2']}}"), "Unrecognized rack name 'rack2'. allowed_racks={\"rack1\"}"),
        (format!("ALTER KEYSPACE r1 WITH replication = {{{nts}, 'datacenter1': ['rack1', 'rack2']}}"), "Unrecognized rack name 'rack2'. allowed_racks={\"rack1\"}"),
        (format!("CREATE KEYSPACE r5 WITH replication = {{{nts}, 'datacenter1': ['rack1', 'rack1']}}"), "Rack list for 'datacenter1' contains duplicate entries"),
        (format!("CREATE KEYSPACE r5 WITH replication = {{{nts}, 'datacenter1': ['rack1']}} AND tablets = {{'enabled': false}}"), "Using rack list for 'datacenter1' is not allowed because the keyspace is not using tablets"),
        (format!("CREATE KEYSPACE r5 WITH replication = {{{nts}, 'datacenter1': []}} AND tablets = {{'enabled': false}}"), "Using rack list for 'datacenter1' is not allowed because the keyspace is not using tablets"),
        ("CREATE KEYSPACE r5 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': ['rack1']} AND tablets = {'enabled': false}".into(), "'replication_factor' option must be numeric."),
        (format!("CREATE KEYSPACE r5 WITH replication = {{{nts}, 'replication_factor': ['rack1']}}"), "Invalid replication factor: ['rack1']: must be a string holding a numerical value"),
        (format!("CREATE KEYSPACE r5 WITH replication = {{{nts}, 'datacenter1': [1]}}"), "Unrecognized rack name '1'. allowed_racks={\"rack1\"}"),
    ] {
        assert_eq!(config(h.err(&cql).await), msg, "{cql}");
    }
    // Lists are only replication values.
    let e = h.err("CREATE TABLE r1.t (p int PRIMARY KEY) WITH compaction = {'class': ['a', 'b']}").await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Syntax, "Invalid map value '[\"a\", \"b\"]' for key 'class'. It should be a simple string."));
    // Cassandra has no rack lists.
    let mut c = H::new(Dialect::Cassandra).await;
    let e = c.err("CREATE KEYSPACE r1 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': ['rack1']}").await;
    assert_eq!(e.code, ErrorCode::Syntax);
}

/// Without datacenters, a new NTS keyspace gets RF 1 in the local one; an ALTER that leaves a
/// datacenter out fails; an unknown class is reported before the vnodes/tablets check.
#[tokio::test]
async fn defaults_and_alter() {
    let mut h = scylla().await;
    for (i, with) in [" WITH replication = {}", " WITH replication = {'class': 'NetworkTopologyStrategy'}", ""].iter().enumerate() {
        h.ok(&format!("CREATE KEYSPACE d{i}{with}")).await;
        assert_eq!(
            replication(&mut h, &format!("d{i}")).await,
            table(&[&["{'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'}", "null"]])
        );
    }
    let e = h.err("ALTER KEYSPACE d0 WITH replication = {'class': 'NetworkTopologyStrategy'}").await;
    assert_eq!(config(e), "Attempted to implicitly drop replicas in datacenter datacenter1. If this is the desired behavior, set replication factor to 0 in datacenter1 explicitly.");
    assert_eq!(config(h.err("ALTER KEYSPACE d0 WITH replication = {'class': 'NoSuchStrategy'}").await), "unable to find class 'NoSuchStrategy'");
    let e = h.err("ALTER KEYSPACE d0 WITH replication = {'class': 'SimpleStrategy'}").await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Invalid, "Cannot alter replication strategy vnode/tablets flavor"));
    // Names (ScyllaDB: up to 192 characters).
    let e = h.err(&format!("CREATE KEYSPACE {} WITH replication = {{}}", "k".repeat(193))).await;
    assert_eq!(e.message, format!("Keyspace names shouldn't be more than 192 characters long (got \"{}\")", "k".repeat(193)));
    h.ok(&format!("CREATE KEYSPACE {}", "k".repeat(192))).await;
}

#[tokio::test]
async fn storage_options() {
    let mut h = scylla().await;
    let rep = "WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': '1'}";
    for (storage, code, msg) in [
        ("{'type': 'S4', 'bucket': '42', 'endpoint': 'localhost'}", ErrorCode::Invalid, "Unknown storage type: S4"),
        ("{'type': 'local'}", ErrorCode::Invalid, "Unknown storage type: local"),
        ("{'type': 'S3', 'bucket': '42', 'endpoint': 'localhost', 'superfluous': 'info'}", ErrorCode::Invalid, "Extraneous options for S3: bucket,endpoint,superfluous; allowed: bucket,endpoint"),
        ("{'type': 'LOCAL', 'superfluous': 'info'}", ErrorCode::Invalid, "Local storage does not accept any custom options"),
        ("{'type': 'S3', 'bucket': '42'}", ErrorCode::Invalid, "Missing S3 option: endpoint"),
        ("{'type': 'S3', 'zz': 'q'}", ErrorCode::Invalid, "Missing S3 option: bucket"),
        ("{'type': 'S3', 'bucket': 'b', 'endpoint': 'localhost'}", ErrorCode::Config, "Endpoint localhost not configured"),
    ] {
        let e = h.err(&format!("CREATE KEYSPACE s1 {rep} AND STORAGE = {storage}")).await;
        assert_eq!((e.code, e.message.as_str()), (code, msg), "{storage}");
    }
    let e = h.err(&format!("CREATE KEYSPACE s1 {rep} AND STORAGE = 'x'")).await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Syntax, "Invalid value for property 'storage'. It should be a map."));
    h.ok(&format!("CREATE KEYSPACE s1 {rep} AND STORAGE = {{'type': 'LOCAL'}}")).await;
    h.ok(&format!("CREATE KEYSPACE s2 {rep} AND STORAGE = {{'bucket': '42'}}")).await;
    h.ok("ALTER KEYSPACE s1 WITH STORAGE = {'type': 'LOCAL'}").await;
    let e = h.err(&format!("ALTER KEYSPACE s1 {rep} AND STORAGE = {{'type': 'S3', 'bucket': '/b1', 'endpoint': 'localhost'}}")).await;
    assert_eq!(e.message, "Cannot alter storage options: LOCAL to S3 is not supported");
    assert!(!desc(&mut h, "KEYSPACE s1").await.contains("storage"));
}

#[tokio::test]
async fn keyspace_tablets_on_alter() {
    let mut h = scylla().await;
    async fn init(h: &mut H, ks: &str) -> String {
        h.rows(&format!("SELECT initial_tablets FROM system_schema.scylla_keyspaces WHERE keyspace_name = '{ks}'"))
            .await[0][0]
            .clone()
    }
    let rep = "replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}";
    h.ok(&format!("CREATE KEYSPACE t WITH {rep} AND tablets = {{'initial': 1}}")).await;
    for (tablets, want) in [
        ("{'initial': 2}", "2"),
        ("{'initial': 0}", "0"),
        ("{'initial': 2}", "2"),
        ("{'enabled': true}", "0"),
        ("{'enabled': true, 'initial': 2}", "2"),
    ] {
        let r = h.ok(&format!("ALTER KEYSPACE t WITH {rep} AND tablets = {tablets}")).await;
        assert!(r.warnings.iter().all(|w| !w.contains("deprecated")), "{tablets}: {:?}", r.warnings);
        assert_eq!(init(&mut h, "t").await, want, "{tablets}");
    }
    // Kept when not given; the short form changes only tablets.
    h.ok(&format!("ALTER KEYSPACE t WITH {rep}")).await;
    assert_eq!(init(&mut h, "t").await, "2");
    h.ok("ALTER KEYSPACE t WITH tablets = {'initial': 3}").await;
    assert_eq!(init(&mut h, "t").await, "3");
    let flavor = "Cannot alter replication strategy vnode/tablets flavor";
    assert_eq!(h.msg("ALTER KEYSPACE t WITH tablets = {'enabled': false}").await, flavor);
    for (t, msg) in [
        ("{'initial': 'x'}", "Initial tablets value should be numeric; found x"),
        ("{'foo': 1}", "Unrecognized tablets option foo"),
        ("{'enabled': 'maybe'}", "Tablets enabled value must be true or false; found: maybe"),
    ] {
        assert_eq!(config(h.err(&format!("ALTER KEYSPACE t WITH tablets = {t}")).await), msg);
        assert_eq!(config(h.err(&format!("CREATE KEYSPACE t2 WITH {rep} AND tablets = {t}")).await), msg);
    }
    h.ok(&format!("CREATE KEYSPACE v WITH {rep} AND tablets = {{'enabled': false}}")).await;
    assert_eq!(h.msg(&format!("ALTER KEYSPACE v WITH {rep} AND tablets = {{'initial': 1}}")).await, flavor);
    assert_eq!(h.msg("ALTER KEYSPACE v WITH tablets = {'enabled': true}").await, flavor);
    h.ok("ALTER KEYSPACE v WITH tablets = {'enabled': false}").await;
}

#[tokio::test]
async fn table_tablet_hints() {
    let mut h = scylla().await;
    h.exec("CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'enabled': true};\n\
            CREATE KEYSPACE v WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'enabled': false};\n\
            CREATE TABLE ks.t (pk int PRIMARY KEY, c int) WITH tablets = {'min_tablet_count': '100'}").await;
    let stored = "SELECT tablets FROM system_schema.scylla_tables WHERE keyspace_name = 'ks' AND table_name = 't'";
    assert_eq!(h.rows(stored).await, table(&[&["{'min_tablet_count': '100'}"]]));
    assert!(desc(&mut h, "TABLE ks.t").await.contains("    AND speculative_retry = '99.0PERCENTILE'\n    AND tablets = {'min_tablet_count': '100'}\n    AND tombstone_gc"));
    h.ok("ALTER TABLE ks.t WITH gc_grace_seconds = 42").await;
    assert!(desc(&mut h, "TABLE ks.t").await.contains("tablets = {'min_tablet_count': '100'}"));
    h.ok("ALTER TABLE ks.t WITH tablets = {}").await;
    assert_eq!(h.rows(stored).await, table(&[&["null"]]));
    assert!(!desc(&mut h, "TABLE ks.t").await.contains("tablets"));
    // Zeroes are stored, not shown.
    h.ok("ALTER TABLE ks.t WITH tablets = {'expected_data_size_in_gb': '0', 'min_tablet_count': '100'}").await;
    assert_eq!(h.rows(stored).await, table(&[&["{'expected_data_size_in_gb': '0', 'min_tablet_count': '100'}"]]));
    assert!(desc(&mut h, "TABLE ks.t").await.contains("tablets = {'min_tablet_count': '100'}\n"));
    h.ok("ALTER TABLE ks.t WITH tablets = {'min_per_shard_tablet_count': '3.14'}").await;
    assert!(desc(&mut h, "TABLE ks.t").await.contains("tablets = {'min_per_shard_tablet_count': '3.14'}"));
    for ok in ["{'min_tablet_count': 'x'}", "{'min_tablet_count': 100}", "{'max_tablet_count': '7'}", "{'expected_data_size_in_gb': '1.5'}"] {
        h.ok(&format!("ALTER TABLE ks.t WITH tablets = {ok}")).await;
    }
    let e = h.err("ALTER TABLE ks.t WITH tablets = {'bogus': '1'}").await;
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Syntax, "Unknown tablet hint 'bogus'"));
    assert_eq!(config(h.err("ALTER TABLE ks.t WITH tablets = {'min_tablet_count': '-1'}").await), "Invalid value '-1' for min_tablet_count");
    let e = h.err("ALTER TABLE ks.t WITH tablets = 'x'").await;
    assert_eq!(e.message, "Invalid value for property 'tablets'. It should be a map.");
    // Reopened.
    h.ok("ALTER TABLE ks.t WITH tablets = {'min_tablet_count': '8'}").await;
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", Dialect::Scylla).await;
    assert_eq!(h.rows(stored).await, table(&[&["{'min_tablet_count': '8'}"]]));
    // A vnodes keyspace takes none, not even `{}`.
    let vnodes = "tablet options cannot be used when tablets are disabled for the keyspace";
    assert_eq!(config(h.err("CREATE TABLE v.t (pk int PRIMARY KEY) WITH tablets = {'min_tablet_count': '100'}").await), vnodes);
    h.ok("CREATE TABLE v.t (pk int PRIMARY KEY)").await;
    assert_eq!(config(h.err("ALTER TABLE v.t WITH tablets = {}").await), vnodes);
}
