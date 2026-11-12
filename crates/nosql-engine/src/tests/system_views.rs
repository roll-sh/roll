//! Generated system tables beyond node identity: Cassandra's `system_views` and
//! `system_virtual_schema`, ScyllaDB's `system.config`, `clients`, `runtime_info`,
//! `token_ring`, `tablets`, topology tables; and the connection registry behind the clients
//! tables. Expected shapes and values are the references' (`docs/nosql/reference/system-tables.md`).

use std::net::SocketAddr;

use super::*;
use crate::ClientInfo;

fn client(port: u16) -> ClientInfo {
    ClientInfo {
        address: Some(SocketAddr::from(([10, 0, 0, 7], port))),
        driver_name: Some("ScyllaDB Python Driver".into()),
        driver_version: Some("3.29.7".into()),
        protocol_version: 4,
        options: vec![
            ("CQL_VERSION".into(), "3.3.1".into()),
            ("DRIVER_NAME".into(), "ScyllaDB Python Driver".into()),
        ],
        client_type: None,
        certificate: None,
    }
}

#[tokio::test]
async fn clients_tables_list_registered_connections() {
    for d in BOTH {
        let mut h = H::new(d).await;
        let a = h.db.register_client(client(5000));
        let b = h.db.register_client(client(5001));
        b.authenticated(Some("alice"));
        b.set_keyspace(Some("ks"));
        b.request();
        b.request();
        b.set_tls(Some(crate::system::clients::Tls {
            protocol: "TLSv1.3".into(),
            cipher_suite: "TLS_AES_256_GCM_SHA384".into(),
        }));
        match d {
            Dialect::Scylla => {
                let rows = h
                    .rows("SELECT address, port, client_type, client_options, connection_stage, driver_name, driver_version, protocol_version, scheduling_group, shard_id, ssl_enabled, ssl_protocol, username FROM system.clients")
                    .await;
                assert_eq!(
                    rows,
                    table(&[
                        &[
                            "10.0.0.7",
                            "5000",
                            "cql",
                            "{'CQL_VERSION': '3.3.1', 'DRIVER_NAME': 'ScyllaDB Python Driver'}",
                            "AUTHENTICATING",
                            "ScyllaDB Python Driver",
                            "3.29.7",
                            "4",
                            "sl:default",
                            "0",
                            "False",
                            "null",
                            "anonymous",
                        ],
                        &[
                            "10.0.0.7",
                            "5001",
                            "cql",
                            "{'CQL_VERSION': '3.3.1', 'DRIVER_NAME': 'ScyllaDB Python Driver'}",
                            "READY",
                            "ScyllaDB Python Driver",
                            "3.29.7",
                            "4",
                            "sl:default",
                            "0",
                            "True",
                            "TLSv1.3",
                            "alice",
                        ],
                    ])
                );
                assert_eq!(
                    h.columns("SELECT * FROM system.clients").await,
                    [
                        "address",
                        "port",
                        "client_type",
                        "client_options",
                        "connection_stage",
                        "driver_name",
                        "driver_version",
                        "hostname",
                        "protocol_version",
                        "scheduling_group",
                        "shard_id",
                        "ssl_cipher_suite",
                        "ssl_enabled",
                        "ssl_protocol",
                        "username"
                    ]
                );
            }
            Dialect::Cassandra => {
                let rows = h
                    .rows("SELECT address, port, connection_stage, keyspace_name, request_count, ssl_cipher_suite, username FROM system_views.clients")
                    .await;
                assert_eq!(
                    rows,
                    table(&[
                        &[
                            "10.0.0.7",
                            "5000",
                            "authenticating",
                            "null",
                            "0",
                            "null",
                            "anonymous"
                        ],
                        &[
                            "10.0.0.7",
                            "5001",
                            "ready",
                            "ks",
                            "2",
                            "TLS_AES_256_GCM_SHA384",
                            "alice"
                        ],
                    ])
                );
                assert_eq!(
                    h.columns("SELECT * FROM system_views.clients").await,
                    [
                        "address",
                        "port",
                        "client_options",
                        "connection_stage",
                        "driver_name",
                        "driver_version",
                        "hostname",
                        "keyspace_name",
                        "protocol_version",
                        "request_count",
                        "ssl_cipher_suite",
                        "ssl_enabled",
                        "ssl_protocol",
                        "username"
                    ]
                );
            }
        }
        drop(a);
        let table_name = if d.is_scylla() {
            "system.clients"
        } else {
            "system_views.clients"
        };
        assert_eq!(
            h.rows(&format!("SELECT port FROM {table_name}")).await,
            table(&[&["5001"]])
        );
        drop(b);
        assert!(
            h.rows(&format!("SELECT port FROM {table_name}"))
                .await
                .is_empty()
        );
    }
}

#[tokio::test]
async fn cassandra_settings_follow_the_configuration() {
    let mut h = H::new(Dialect::Cassandra).await;
    let all = h.rows("SELECT name FROM system_views.settings").await;
    assert_eq!(all.len(), 638);
    let get = |name: &str| format!("SELECT value FROM system_views.settings WHERE name = '{name}'");
    assert_eq!(
        h.rows(&get("batch_size_warn_threshold")).await,
        table(&[&["5KiB"]])
    );
    assert_eq!(
        h.rows(&get("batch_size_fail_threshold_in_kb")).await,
        table(&[&["50"]])
    );
    assert_eq!(
        h.rows(&get("cluster_name")).await,
        table(&[&["Test Cluster"]])
    );
    assert_eq!(
        h.rows(&get("listen_address")).await,
        table(&[&["127.0.0.1"]])
    );
    assert_eq!(
        h.rows(&get("authenticator")).await,
        table(&[&["AllowAllAuthenticator"]])
    );
    assert_eq!(h.rows(&get("num_tokens")).await, table(&[&["16"]]));
    assert_eq!(
        h.rows(&get("permissions_validity")).await,
        table(&[&["2000ms"]])
    );
    assert_eq!(
        h.rows(&get("allocate_tokens_for_keyspace")).await,
        table(&[&["null"]])
    );
    assert_eq!(
        h.specs("SELECT * FROM system_views.settings").await,
        [
            ("name".to_string(), "text".to_string()),
            ("value".into(), "text".into())
        ]
    );
    // Another configuration.
    let store = nosql_storage::store::Store::start(
        nosql_storage::test_objects("engine"),
        small_store_config(),
    )
    .await
    .unwrap();
    let handle = store
        .open("db2", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db2", Dialect::Cassandra);
    cfg.cluster_name = "roll".into();
    cfg.settings.batch_size_warn_threshold = 8 * 1024;
    cfg.settings.authenticator = "PasswordAuthenticator".into();
    cfg.settings.materialized_views_enabled = true;
    let db = Database::open(cfg, handle).await.unwrap();
    let mut s = db.connect(Default::default());
    s.authenticate(b"\0cassandra\0cassandra").await.unwrap();
    let mut h2 = H { db, s, store };
    assert_eq!(h2.rows(&get("cluster_name")).await, table(&[&["roll"]]));
    assert_eq!(
        h2.rows(&get("batch_size_warn_threshold")).await,
        table(&[&["8KiB"]])
    );
    assert_eq!(
        h2.rows(&get("authenticator")).await,
        table(&[&["PasswordAuthenticator"]])
    );
    assert_eq!(
        h2.rows(&get("materialized_views_enabled")).await,
        table(&[&["true"]])
    );
}

#[tokio::test]
async fn cassandra_views_of_a_fresh_node() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.exec(&format!("{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY)"))
        .await;
    assert_eq!(
        h.rows("SELECT * FROM system_views.caches").await,
        table(&[
            &["counters", "16777216", "0", "0", "NaN", "0", "0", "0", "0"],
            &["keys", "33554432", "0", "0", "NaN", "0", "0", "0", "0"],
            &["rows", "0", "0", "0", "NaN", "0", "0", "0", "0"],
        ])
    );
    let pools = h
        .rows("SELECT name, active_tasks_limit FROM system_views.thread_pools")
        .await;
    assert_eq!(pools.len(), 21);
    assert!(pools.contains(&vec!["ReadStage".to_string(), "32".to_string()]));
    // One row per stored table (48 built-in ones plus ks.t).
    let lat = h
        .rows(
            "SELECT keyspace_name, table_name, count, max_ms FROM system_views.local_read_latency",
        )
        .await;
    assert_eq!(lat.len(), 49);
    assert!(
        lat.iter().any(|r| r[0] == "ks" && r[1] == "t" && r[2] == "0"),
        "{lat:?}"
    );
    for t in [
        "coordinator_read_latency",
        "coordinator_scan_latency",
        "coordinator_write_latency",
        "local_scan_latency",
        "local_write_latency",
        "disk_usage",
        "max_partition_size",
        "max_sstable_size",
        "max_sstable_duration",
        "rows_per_read",
        "tombstones_per_read",
    ] {
        assert_eq!(
            h.rows(&format!("SELECT * FROM system_views.{t}"))
                .await
                .len(),
            49,
            "{t}"
        );
    }
    assert_eq!(
        h.rows("SELECT name FROM system_views.batch_metrics").await,
        table(&[
            &["partitions_per_counter_batch"],
            &["partitions_per_logged_batch"],
            &["partitions_per_unlogged_batch"],
        ])
    );
    let gossip = h
        .rows(
            "SELECT address, port, dc, rack, release_version, status FROM system_views.gossip_info",
        )
        .await;
    assert_eq!(gossip.len(), 1);
    assert_eq!(
        gossip[0][..5],
        ["127.0.0.1", "7000", "datacenter1", "rack1", "5.0.9"]
    );
    assert!(gossip[0][5].starts_with("NORMAL,"));
    assert_eq!(
        h.rows("SELECT value FROM system_views.system_properties WHERE name = 'java.version'")
            .await,
        table(&[&["17.0.20.1"]])
    );
    for empty in [
        "sstable_tasks",
        "queries",
        "pending_hints",
        "streaming",
        "repairs",
        "snapshots",
        "roles_cache_keys",
        "internode_inbound",
    ] {
        assert!(
            h.rows(&format!("SELECT * FROM system_views.{empty}"))
                .await
                .is_empty(),
            "{empty}"
        );
    }
    // cql_metrics counts the prepared statements.
    h.s.prepare("SELECT k FROM ks.t WHERE k = ?", None)
        .await
        .unwrap();
    assert_eq!(
        h.rows(
            "SELECT value FROM system_views.cql_metrics WHERE name = 'prepared_statements_count'"
        )
        .await,
        table(&[&["1"]])
    );
    assert_eq!(
        h.rows("SELECT logged_keyspace, query_string FROM system.prepared_statements")
            .await,
        table(&[&["null", "SELECT k FROM ks.t WHERE k = ?"]])
    );
}

#[tokio::test]
async fn virtual_schema_lists_the_virtual_tables() {
    let mut h = H::new(Dialect::Cassandra).await;
    assert_eq!(
        h.rows("SELECT * FROM system_virtual_schema.keyspaces")
            .await,
        table(&[&["system_views"], &["system_virtual_schema"]])
    );
    let tables = h.rows("SELECT * FROM system_virtual_schema.tables").await;
    assert_eq!(tables.len(), 46);
    assert_eq!(
        tables[0],
        [
            "system_views",
            "batch_metrics",
            "Metrics specific to batch statements"
        ]
    );
    let cols = h.rows("SELECT * FROM system_virtual_schema.columns").await;
    assert_eq!(cols.len(), 457);
    assert!(
        cols.contains(
            &[
                "system_views",
                "settings",
                "name",
                "none",
                "0x6e616d65",
                "partition_key",
                "0",
                "text"
            ]
            .map(String::from)
            .to_vec()
        )
    );
    assert!(
        cols.contains(
            &[
                "system_views",
                "clients",
                "port",
                "asc",
                "0x706f7274",
                "clustering",
                "0",
                "int"
            ]
            .map(String::from)
            .to_vec()
        )
    );
    // Virtual keyspaces are not in system_schema.
    assert!(
        h.rows("SELECT * FROM system_schema.keyspaces WHERE keyspace_name = 'system_views'")
            .await
            .is_empty()
    );
    assert!(
        h.rows("SELECT * FROM system_schema.tables WHERE keyspace_name = 'system_virtual_schema'")
            .await
            .is_empty()
    );
    // ScyllaDB has none.
    let mut s = H::new(Dialect::Scylla).await;
    assert_eq!(
        s.msg("SELECT * FROM system_virtual_schema.keyspaces").await,
        "Keyspace system_virtual_schema does not exist"
    );
    assert_eq!(
        s.msg("SELECT * FROM system_views.settings").await,
        "Keyspace system_views does not exist"
    );
}

#[tokio::test]
async fn scylla_config_follows_the_configuration() {
    let mut h = H::new(Dialect::Scylla).await;
    assert_eq!(h.rows("SELECT name FROM system.config").await.len(), 415);
    let get =
        |name: &str| format!("SELECT source, type, value FROM system.config WHERE name = '{name}'");
    assert_eq!(
        h.rows(&get("batch_size_warn_threshold_in_kb")).await,
        table(&[&["config", "integer", "128"]])
    );
    assert_eq!(
        h.rows(&get("authenticator")).await,
        table(&[&[
            "default",
            "string",
            "\"org.apache.cassandra.auth.AllowAllAuthenticator\""
        ]])
    );
    assert_eq!(
        h.rows(&get("cluster_name")).await,
        table(&[&["default", "string", "\"\""]])
    );
    assert_eq!(
        h.rows(&get("rpc_address")).await,
        table(&[&["cli", "string", "\"127.0.0.1\""]])
    );
    assert_eq!(
        h.rows(&get("tablets_mode_for_new_keyspaces")).await,
        table(&[&["config", "tablets mode", "\"enabled\""]])
    );
    assert_eq!(
        h.rows(&get("enable_user_defined_functions")).await,
        table(&[&["default", "bool", "false"]])
    );
    // A database with other settings.
    let store = nosql_storage::store::Store::start(
        nosql_storage::test_objects("engine"),
        small_store_config(),
    )
    .await
    .unwrap();
    let handle = store
        .open("db2", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db2", Dialect::Scylla);
    cfg.cluster_name = "roll".into();
    cfg.settings.user_defined_functions_enabled = true;
    cfg.settings.batch_size_warn_threshold = 64 * 1024;
    let db = Database::open(cfg, handle).await.unwrap();
    let s = db.connect(Default::default());
    let mut h2 = H { db, s, store };
    assert_eq!(
        h2.rows(&get("cluster_name")).await,
        table(&[&["config", "string", "\"roll\""]])
    );
    assert_eq!(
        h2.rows(&get("enable_user_defined_functions")).await,
        table(&[&["config", "bool", "true"]])
    );
    assert_eq!(
        h2.rows(&get("experimental_features")).await,
        table(&[&["config", "experimental features", "[\"udf\"]"]])
    );
    assert_eq!(
        h2.rows(&get("batch_size_warn_threshold_in_kb")).await,
        table(&[&["config", "integer", "64"]])
    );
}

#[tokio::test]
async fn scylla_node_tables() {
    let mut h = H::new(Dialect::Scylla).await;
    h.exec(
        "CREATE KEYSPACE tk WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1};\n\
         CREATE TABLE tk.t (k int PRIMARY KEY);\n\
         CREATE KEYSPACE vk WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'enabled': false};\n\
         CREATE TABLE vk.t (k int PRIMARY KEY)",
    )
    .await;
    let host = crate::uuid::format(&h.db.config().host_id());
    // Tablets keyspaces: 16 tablets per table, one replica.
    let tablets = h
        .rows("SELECT last_token, keyspace_name, table_name, tablet_count, resize_type, replicas FROM system.tablets")
        .await;
    assert_eq!(tablets.len(), 16);
    assert_eq!(
        tablets[0],
        [
            "-8070450532247928833".to_string(),
            "tk".into(),
            "t".into(),
            "16".into(),
            "none".into(),
            format!("[({host}, 0)]"),
        ]
    );
    assert_eq!(tablets[15][0], "9223372036854775807");
    // The token ring: vnode keyspaces as <ALL>, tablets per table.
    let ring = h
        .rows("SELECT table_name, endpoint, dc, rack FROM system.token_ring WHERE keyspace_name = 'vk'")
        .await;
    assert_eq!(ring.len(), 256);
    assert_eq!(ring[0], ["<ALL>", "127.0.0.1", "datacenter1", "rack1"]);
    let ring = h
        .rows("SELECT table_name, start_token, end_token FROM system.token_ring WHERE keyspace_name = 'tk'")
        .await;
    assert_eq!(ring.len(), 16);
    // start_token is a text clustering column: rows sort as strings.
    assert!(ring.contains(
        &["t", "-9223372036854775808", "-8070450532247928833"]
            .map(String::from)
            .to_vec()
    ));
    assert!(
        h.rows("SELECT * FROM system.token_ring WHERE keyspace_name = 'system'")
            .await
            .is_empty()
    );
    assert_eq!(
        h.rows("SELECT * FROM system.protocol_servers").await,
        table(&[
            &[
                "native transport",
                "['127.0.0.1:9042', '127.0.0.1:19042']",
                "cql",
                "3.3.1"
            ],
            &["alternator", "['127.0.0.1:8000']", "dynamodb", "2012-08-10"],
        ])
    );
    assert_eq!(
        h.rows(
            "SELECT peer, dc, host_id, owns, rack, status, tokens, up FROM system.cluster_status"
        )
        .await,
        table(&[&[
            "127.0.0.1",
            "datacenter1",
            &host,
            "1",
            "rack1",
            "NORMAL",
            "256",
            "True"
        ]])
    );
    assert_eq!(
        h.rows("SELECT node, ip, tablets_allocated, up FROM system.load_per_node")
            .await,
        table(&[&[&host, "127.0.0.1", "16", "True"]])
    );
    let topo = h
        .rows("SELECT key, host_id, node_state, datacenter, rack, release_version, num_tokens, shard_count, upgrade_state FROM system.topology")
        .await;
    assert_eq!(
        topo,
        table(&[&[
            "topology",
            &host,
            "normal",
            "datacenter1",
            "rack1",
            "3.0.8",
            "256",
            "1",
            "done"
        ]])
    );
    let rt = h.rows("SELECT group, item FROM system.runtime_info").await;
    assert_eq!(rt.len(), 22);
    assert_eq!(rt[0], ["cache", "entries"]);
    assert_eq!(
        h.rows("SELECT key, ip_addr FROM system.discovery").await,
        table(&[&["peers", "127.0.0.1"]])
    );
    assert_eq!(
        h.rows("SELECT disposition, can_vote FROM system.raft_state")
            .await,
        table(&[&["CURRENT", "True"]])
    );
    for empty in [
        "snapshots",
        "large_partitions",
        "large_rows",
        "large_cells",
        "compaction_history",
    ] {
        assert!(
            h.rows(&format!("SELECT * FROM system.{empty}"))
                .await
                .is_empty(),
            "{empty}"
        );
    }
}

#[tokio::test]
async fn system_tables_have_the_reference_options() {
    for d in BOTH {
        let mut h = H::new(d).await;
        let rows = h
            .rows("SELECT comment, gc_grace_seconds, default_time_to_live FROM system_schema.tables WHERE keyspace_name = 'system_traces' AND table_name = 'sessions'")
            .await;
        match d {
            Dialect::Scylla => assert_eq!(rows, table(&[&["", "864000", "86400"]])),
            Dialect::Cassandra => assert_eq!(rows, table(&[&["tracing sessions", "0", "0"]])),
        }
        let n = h
            .rows("SELECT table_name FROM system_schema.tables WHERE keyspace_name = 'system'")
            .await
            .len();
        assert_eq!(n, if d.is_scylla() { 61 } else { 23 });
        if d.is_scylla() {
            assert_eq!(
                h.rows("SELECT type_name FROM system_schema.types WHERE keyspace_name = 'system'")
                    .await,
                table(&[
                    &["repair_scheduler_config"],
                    &["sstableinfo"],
                    &["tablet_task_info"]
                ])
            );
        }
    }
}
