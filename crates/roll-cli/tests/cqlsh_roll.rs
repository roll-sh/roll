//! The golden corpus against roll itself: an in-process `nosql-server` (single mode) per dialect,
//! with our cqlsh, compared with what each reference's cqlsh printed (`tests/cqlsh/golden`).
//!
//! A mismatch here is a difference between roll's server and the reference (the cqlsh side is
//! pinned by `tests/cqlsh.rs`), so by default this reports the cases that differ and checks
//! that every case still runs; `ROLL_CQLSH_ROLL_STRICT=1` makes any difference fail. Needs
//! `--features roll-server-tests`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;

fn serve(dialect: nosql_engine::Dialect) -> u16 {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let cfg = nosql_storage::store::StoreConfig { maintenance_interval: Duration::from_millis(200), ..Default::default() };
            let store = nosql_storage::store::Store::start(nosql_storage::test_objects("cqlsh"), cfg).await.unwrap();
            let handle = store.open("db1", Default::default()).await.unwrap();
            let db = nosql_engine::Database::open(nosql_engine::DbConfig::new("db1", dialect), handle).await.unwrap();
            let dir: Arc<dyn nosql_server::Directory> = Arc::new(nosql_server::Single::new(db));
            let addr = nosql_server::net::start(dir, nosql_server::ListenOptions::default()).await.unwrap();
            tx.send(addr.port()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    rx.recv_timeout(Duration::from_secs(60)).expect("server started")
}

fn against_roll(dialect: &'static str, d: nosql_engine::Dialect) {
    let r = Ref { dialect, container: "", host: "127.0.0.1", port: serve(d) };
    stage_files(&r, false);
    let mut same = 0;
    let mut diffs = vec![];
    for (name, args) in cases() {
        reset_scratch(&r, false);
        let want = read_golden(&r, &name);
        let got = ours(&r, &args, None);
        assert!(!got.err.contains("Connection error"), "{dialect} {name}: {}", got.err);
        match compare(&mask(&want), &mask(&got)) {
            None => same += 1,
            Some(d) => diffs.push(format!("{name}: {d}")),
        }
    }
    eprintln!("roll ({dialect}): {same} of {} cases print what the reference printed", same + diffs.len());
    for d in &diffs {
        eprintln!("  {d}");
    }
    if std::env::var("ROLL_CQLSH_ROLL_STRICT").is_ok_and(|v| v == "1") {
        assert!(diffs.is_empty(), "{} cases differ", diffs.len());
    }
}

#[test]
fn roll_scylla() {
    against_roll("scylla", nosql_engine::Dialect::Scylla);
}

#[test]
fn roll_cassandra() {
    against_roll("cassandra", nosql_engine::Dialect::Cassandra);
}
