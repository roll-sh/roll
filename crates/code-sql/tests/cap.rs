//! A process's cap on connections to one server (`code_sql::limit_connections`, rolld's
//! `ROLL_SYSTEM_DB_CONNECTIONS`) holds across every pool and LISTEN connection to it. Its own
//! binary: the cap is per process. Postgres only (`ROLL_CODE_TEST_DATABASE`).

use code_sql::Db;
use std::sync::atomic::{AtomicI64, Ordering};

#[test]
fn pools_to_one_server_share_its_connection_cap() {
    let Some(url) = std::env::var(code_sql::TEST_DATABASE_ENV).ok().filter(|u| !u.is_empty()) else { return };
    code_sql::limit_connections(&url, 3).unwrap();
    let prefix = format!("t_cap_{}", std::process::id());
    // More pools than the cap, each allowed more than the cap on its own.
    let pools: Vec<Db> = (0..5).map(|i| Db::connect(&url, &format!("{prefix}_{i}"), code_sql::PgOptions { max_connections: 4, ..Default::default() }).unwrap()).collect();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let _listening = rt.block_on(async { pools[0].listen("cap_test").unwrap() });
    for db in &pools {
        db.ensure_schema("CREATE TABLE IF NOT EXISTS n (k INTEGER PRIMARY KEY, v INTEGER NOT NULL)").unwrap();
        db.execute("INSERT INTO n (k, v) VALUES (1, 0)", ()).unwrap();
    }
    let most = AtomicI64::new(0);
    let count = format!("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name LIKE 'roll-code {prefix}%'");
    std::thread::scope(|s| {
        for t in 0..10 {
            let (db, most, count) = (&pools[t % pools.len()], &most, &count);
            s.spawn(move || {
                for _ in 0..20 {
                    db.write(|c| c.execute("UPDATE n SET v = v + 1 WHERE k = 1", ())).unwrap();
                    // Counted from inside: this connection is one of them.
                    let open: i64 = db.query_row(count, (), |r| r.get(0)).unwrap();
                    most.fetch_max(open, Ordering::Relaxed);
                }
            });
        }
    });
    let most = most.into_inner();
    assert!((2..=3).contains(&most), "{most} connections open at most (the cap is 3, one of them listening)");
    let total: i64 = pools.iter().map(|db| db.query_row("SELECT v FROM n WHERE k = 1", (), |r| r.get::<_, i64>(0)).unwrap()).sum();
    assert_eq!(total, 200);
    for (i, db) in pools.iter().enumerate() {
        db.write(|c| c.execute_batch(&format!("DROP SCHEMA {prefix}_{i} CASCADE"))).unwrap();
    }
}
