//! Memtables that stay full (flushes can't keep up) time writes out as both references do, at
//! the statement's consistency; the database's memtable cap shows in `system_views.settings`.

use std::time::Duration;

use nosql_storage::store::{Store, StoreConfig};

use super::*;
use crate::error::{Consistency, ErrorDetail};

async fn harness(d: Dialect, db_cap: u64) -> H {
    let cfg = StoreConfig {
        memtable_db_bytes: db_cap,
        write_timeout: Duration::from_millis(100),
        ..small_store_config()
    };
    let store = Store::start(nosql_storage::test_objects("engine-memory"), cfg)
        .await
        .unwrap();
    H::on(store, "db", d).await
}

fn quorum(cql: &str) -> (String, QueryParams) {
    let p = QueryParams {
        consistency: Consistency::Quorum,
        ..Default::default()
    };
    (cql.to_string(), p)
}

/// Runs `cql` (with `{i}` replaced) until it fails.
async fn until_error(h: &mut H, cql: &str) -> Error {
    for i in 0..100_000 {
        let (q, p) = quorum(&cql.replace("{i}", &i.to_string()));
        if let Err(e) = h.s.query(&q, p).await {
            return e;
        }
    }
    panic!("{cql}: never timed out");
}

#[tokio::test]
async fn full_memtables_time_writes_out() {
    for d in BOTH {
        let mut h = harness(d, 2 << 20).await;
        h.exec(
            "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};\n\
             CREATE TABLE ks.t (p int, c int, v text, PRIMARY KEY (p, c))",
        )
        .await;
        let flushes = h.store.config().flush_concurrency as u32;
        let stalled = h.store.env.flushes.clone().acquire_many_owned(flushes).await.unwrap();
        let payload = "x".repeat(2000);
        let e = until_error(&mut h, &format!("INSERT INTO ks.t (p, c, v) VALUES ({{i}}, 1, '{payload}')")).await;
        let want = if d == Dialect::Scylla {
            "Operation timed out for ks.t - received only 0 responses from 1 CL=QUORUM."
        } else {
            "Operation timed out - received only 0 responses."
        };
        assert_eq!((e.code, e.message.as_str()), (ErrorCode::WriteTimeout, want), "{d}");
        assert!(
            matches!(&e.detail, ErrorDetail::WriteTimeout { consistency: Consistency::Quorum, received: 0, block_for: 1, write_type, .. } if write_type == "SIMPLE"),
            "{d}: {:?}",
            e.detail
        );
        // A logged batch over partitions times out writing its batch log.
        let e = until_error(
            &mut h,
            &format!("BEGIN BATCH INSERT INTO ks.t (p, c, v) VALUES ({{i}}, 2, '{payload}'); INSERT INTO ks.t (p, c, v) VALUES (-1{{i}}, 2, 'y'); APPLY BATCH"),
        )
        .await;
        assert_eq!(e.code, ErrorCode::WriteTimeout, "{d}");
        assert!(
            matches!(&e.detail, ErrorDetail::WriteTimeout { consistency: Consistency::Quorum, write_type, .. } if write_type == "BATCH_LOG"),
            "{d}: {:?}",
            e.detail
        );
        if d == Dialect::Scylla {
            assert_eq!(
                e.message,
                "Operation timed out for system.batchlog_v2 - received only 0 responses from 1 CL=QUORUM."
            );
        }
        // Flushes run again: writes go in.
        drop(stalled);
        let mut ok = false;
        for _ in 0..50 {
            if h.run("INSERT INTO ks.t (p, c, v) VALUES (0, 9, 'after')").await.is_ok() {
                ok = true;
                break;
            }
        }
        assert!(ok, "{d}");
        h.ok("SELECT v FROM ks.t WHERE p = 0 AND c = 9").await;
    }
}

#[tokio::test]
async fn the_memtable_cap_shows_in_the_settings() {
    let store = Store::start(nosql_storage::test_objects("engine-memory-settings"), small_store_config())
        .await
        .unwrap();
    let cfg = nosql_storage::db::DbConfig {
        memtable_space_bytes: Some(24 << 20),
        ..Default::default()
    };
    let handle = store.open("db", cfg).await.unwrap();
    let db = Database::open(DbConfig::new("db", Dialect::Cassandra), handle).await.unwrap();
    let mut h = H { s: db.connect(Default::default()), db, store };
    let q = |name: &str| format!("SELECT value FROM system_views.settings WHERE name = '{name}'");
    assert_eq!(h.rows(&q("memtable_heap_space")).await, vec![vec!["24MiB".to_string()]]);
    assert_eq!(h.rows(&q("memtable_heap_space_in_mb")).await, vec![vec!["24".to_string()]]);
    // A plan change applies at once.
    h.db.store().set_config(nosql_storage::db::DbConfig {
        memtable_space_bytes: Some(48 << 20),
        ..Default::default()
    });
    assert_eq!(h.rows(&q("memtable_heap_space")).await, vec![vec!["48MiB".to_string()]]);
}
