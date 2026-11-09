//! System tables: `system.local` / `peers`, `system_schema` built-ins, virtual tables.

use super::*;

#[tokio::test]
async fn local_describes_this_node() {
    for d in BOTH {
        let mut h = H::new(d).await;
        let r = h
            .rows("SELECT key, data_center, rack, partitioner, cql_version FROM system.local")
            .await;
        assert_eq!(r.len(), 1, "{d}");
        assert_eq!(r[0][0], "local");
        assert_eq!(r[0][1], "datacenter1");
        assert_eq!(r[0][2], "rack1");
        assert_eq!(r[0][3], "org.apache.cassandra.dht.Murmur3Partitioner");
        let tokens = h.rows("SELECT tokens FROM system.local").await;
        assert!(tokens[0][0].starts_with('{'), "{d}: {tokens:?}");
        assert!(h.rows("SELECT * FROM system.peers").await.is_empty(), "{d}");
    }
}

#[tokio::test]
async fn builtin_keyspaces_are_listed_and_read_only() {
    for d in BOTH {
        let mut h = H::new(d).await;
        let ks: Vec<String> = h
            .rows("SELECT keyspace_name FROM system_schema.keyspaces")
            .await
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        for k in [
            "system",
            "system_schema",
            "system_auth",
            "system_distributed",
            "system_traces",
        ] {
            assert!(ks.contains(&k.to_string()), "{d}: {k} missing from {ks:?}");
        }
        let cols = h.rows("SELECT column_name FROM system_schema.columns WHERE keyspace_name = 'system_schema' AND table_name = 'tables'").await;
        assert!(cols.iter().any(|c| c[0] == "table_name"), "{d}");
        assert_eq!(
            code(&h.err("CREATE TABLE system.x (pk int PRIMARY KEY)").await),
            ErrorCode::Unauthorized,
            "{d}"
        );
        assert!(h.run("DROP KEYSPACE system_schema").await.is_err(), "{d}");
        assert!(
            h.run("INSERT INTO system.local (key) VALUES ('x')")
                .await
                .is_err(),
            "{d}"
        );
    }
}

/// `system.size_estimates` (and Cassandra's `table_estimates`) count a table's partitions,
/// split over the ring's ranges by token width (`test_system_tables.py::test_partitions_estimate_*`).
#[tokio::test]
async fn size_estimates_count_partitions() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (p int PRIMARY KEY, v text)"
        ))
        .await;
        for i in 0..300 {
            h.ok(&format!(
                "INSERT INTO ks.t (p, v) VALUES ({i}, 'value {i}')"
            ))
            .await;
        }
        let (ranges, sum, sized) = estimates(&mut h).await;
        assert!(ranges > 1, "{d}");
        assert_eq!(sum, 300, "{d}: memtables");
        assert!(sized, "{d}");
        h.db.flush().await.unwrap();
        let (_, sum, _) = estimates(&mut h).await;
        assert_eq!(sum, 300, "{d}: flushed");
    }
}

/// Ranges, summed `partitions_count` and whether a size is set, for `ks.t`.
async fn estimates(h: &mut H) -> (usize, i64, bool) {
    let rows = h
        .rows("SELECT partitions_count, mean_partition_size FROM system.size_estimates WHERE keyspace_name = 'ks' AND table_name = 't'")
        .await;
    let sum: i64 = rows.iter().map(|r| r[0].parse::<i64>().unwrap()).sum();
    let sized = rows.iter().any(|r| r[1] != "0");
    (rows.len(), sum, sized)
}

/// ScyllaDB stores `system.scylla_table_schema_history` and lets a superuser write it (the
/// Alternator suite does, then pages through it); roll stores it too. Generated tables still
/// refuse writes.
#[tokio::test]
async fn schema_history_is_writable() {
    let mut h = H::new(Dialect::Scylla).await;
    let insert = |c: u32| {
        format!(
            "INSERT INTO system.scylla_table_schema_history (cf_id, schema_version, column_name, clustering_order, column_name_bytes, kind, position, type) VALUES (eee7eb26-a372-4eb4-aeaa-72f224cf0000, eee7eb26-a372-4eb4-aeaa-82f224cf{c:04}, 'a', 'a', 0x1234, 'k', 1, 't')"
        )
    };
    for c in 0..3 {
        h.ok(&insert(c)).await;
    }
    h.ok("DELETE FROM system.scylla_table_schema_history WHERE cf_id = eee7eb26-a372-4eb4-aeaa-72f224cf0000 AND schema_version = eee7eb26-a372-4eb4-aeaa-82f224cf0001")
        .await;
    assert_eq!(
        h.rows("SELECT schema_version FROM system.scylla_table_schema_history WHERE cf_id = eee7eb26-a372-4eb4-aeaa-72f224cf0000")
            .await,
        vec![
            vec!["eee7eb26-a372-4eb4-aeaa-82f224cf0000".to_string()],
            vec!["eee7eb26-a372-4eb4-aeaa-82f224cf0002".to_string()],
        ]
    );
    let e = h.err("INSERT INTO system.local (key) VALUES ('x')").await;
    assert_eq!(e.message, "system keyspace is not user-modifiable.");
}

/// A CDC table and its log are one tablet group: `ALTER TABLE <log> WITH tablets` changes the
/// group's tablet count (`system.tablets`, the log naming its `base_table`), and every change
/// starts a stream generation (`system.cdc_timestamps`, `system.cdc_streams`).
#[tokio::test]
async fn cdc_tablet_resizes() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(
        "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1} AND tablets = {'initial': 2}\n;\n\
         CREATE TABLE ks.t (p int PRIMARY KEY, v int) WITH cdc = {'enabled': true}",
    )
    .await;
    let ids = h
        .rows("SELECT id FROM system_schema.tables WHERE keyspace_name = 'ks' AND table_name IN ('t', 't_scylla_cdc_log')")
        .await;
    let (base, log) = (ids[0][0].clone(), ids[1][0].clone());
    async fn tablets(h: &mut H, id: &str) -> Vec<Vec<String>> {
        h.rows(&format!(
            "SELECT base_table, tablet_count FROM system.tablets WHERE table_id = {id} LIMIT 1"
        ))
        .await
    }
    assert_eq!(tablets(&mut h, &log).await, vec![vec![base.clone(), "2".to_string()]]);
    assert_eq!(tablets(&mut h, &base).await, vec![vec!["null".to_string(), "2".to_string()]]);
    /// The stream count of each generation, newest first (the table's clustering order).
    async fn generations(h: &mut H) -> Vec<String> {
        let ts = h
            .rows("SELECT toUnixTimestamp(timestamp) FROM system.cdc_timestamps WHERE keyspace_name = 'ks' AND table_name = 't'")
            .await;
        let mut counts = Vec::new();
        for t in &ts {
            let n = h
                .rows(&format!(
                    "SELECT count(*) FROM system.cdc_streams WHERE keyspace_name = 'ks' AND table_name = 't' AND timestamp = {} AND stream_state = 0",
                    t[0]
                ))
                .await;
            counts.push(n[0][0].clone());
        }
        counts
    }
    assert_eq!(generations(&mut h).await, vec!["2"]);
    h.ok("ALTER TABLE ks.t_scylla_cdc_log WITH tablets = {'min_tablet_count': 8}").await;
    assert_eq!(tablets(&mut h, &base).await, vec![vec!["null".to_string(), "8".to_string()]]);
    // Not a change of the group's count: no generation.
    h.ok("ALTER TABLE ks.t WITH tablets = {'min_tablet_count': 4}").await;
    h.ok("ALTER TABLE ks.t WITH tablets = {'min_tablet_count': 16}").await;
    assert_eq!(generations(&mut h).await, vec!["16", "8", "2"]);
    // Writes go to the current generation's streams.
    h.ok("INSERT INTO ks.t (p, v) VALUES (1, 1)").await;
    let current = h
        .rows("SELECT stream_id FROM system.cdc_streams WHERE keyspace_name = 'ks' AND table_name = 't' ALLOW FILTERING")
        .await;
    let logged = h.rows("SELECT \"cdc$stream_id\" FROM ks.t_scylla_cdc_log").await;
    assert!(current[current.len() - 16..].contains(&logged[0]), "{logged:?}");
}
