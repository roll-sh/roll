//! Application-style concurrency, beyond the isolation suite's scripted interleavings: many
//! sessions at once running the patterns applications rely on (a job queue's SKIP LOCKED
//! fetch, upsert storms, ON CONFLICT DO NOTHING under an exclusion constraint, SERIALIZABLE
//! transfers retried on serialization failures, advisory-lock leader election, LISTEN /
//! NOTIFY delivery order). Each check asserts what PostgreSQL guarantees: no lost or doubled
//! work, no deadlock PostgreSQL wouldn't have, the invariants of the data.
//!
//! The checks run against the engine; `against_reference` (ignored) runs the same ones against
//! a PostgreSQL 18.6, which must pass them too:
//!
//!     APPCONC_REF=127.0.0.1:55499 APPCONC_PASSWORD=pw \
//!       cargo test -p pg-engine --test app_concurrency -- --ignored --nocapture

use futures::StreamExt;
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_postgres::error::SqlState;
use tokio_postgres::{AsyncMessage, Client, Config, NoTls};

/// Sessions per check.
const SESSIONS: usize = 12;

/// Where the checks connect: the engine's server or a reference, database `appconc`.
#[derive(Clone)]
struct Target(Config);

impl Target {
    async fn connect(&self) -> Client {
        let (client, conn) = self.0.connect(NoTls).await.expect("connect");
        tokio::spawn(conn);
        client
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
}

/// Run `check` against a fresh engine (its own server and database).
fn on_engine<F, Fut>(check: F)
where
    F: FnOnce(Target) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions {
        data_dir: dir.path().to_path_buf(),
        cache_bytes: 16 << 20,
        memtable_bytes: 4 << 20,
        sync: false,
        idle_close: Duration::from_secs(60),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap();
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust })
        .unwrap()
        .to_string();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let mut c = Config::new();
    c.host(host).port(port.parse().unwrap()).user("postgres").dbname("postgres");
    runtime().block_on(async {
        let admin = Target(c.clone()).connect().await;
        admin.batch_execute("CREATE DATABASE appconc").await.unwrap();
        c.dbname("appconc");
        check(Target(c)).await;
    });
    engine.shutdown();
}

/// A deterministic pseudo-random sequence per session.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// SKIP LOCKED job queues (Oban's `UPDATE ... WHERE id IN (SELECT ... FOR UPDATE SKIP
/// LOCKED)`, que's / graphile-worker's CTE): every job is taken exactly once.
async fn job_queue(t: Target) {
    let c = t.connect().await;
    c.batch_execute(
        "DROP TABLE IF EXISTS jobs;
         CREATE TABLE jobs (id int PRIMARY KEY, state text NOT NULL DEFAULT 'available', priority int NOT NULL, worker int);
         CREATE INDEX jobs_fetch ON jobs (state, priority, id);
         INSERT INTO jobs (id, priority) SELECT g, g % 7 FROM generate_series(1, 1500) g;
         ANALYZE jobs",
    )
    .await
    .unwrap();
    let mut tasks = Vec::new();
    for w in 0..SESSIONS as i32 {
        let t = t.clone();
        tasks.push(tokio::spawn(async move {
            let c = t.connect().await;
            let mut got = Vec::new();
            loop {
                let rows = if w % 2 == 0 {
                    c.query(
                        "UPDATE jobs SET state = 'done', worker = $1 WHERE id IN
                           (SELECT id FROM jobs WHERE state = 'available' ORDER BY priority, id LIMIT 3 FOR UPDATE SKIP LOCKED)
                         RETURNING id",
                        &[&w],
                    )
                    .await
                } else {
                    c.query(
                        "WITH j AS (SELECT id FROM jobs WHERE state = 'available' ORDER BY priority, id LIMIT 1 FOR UPDATE SKIP LOCKED)
                         UPDATE jobs SET state = 'done', worker = $1 FROM j WHERE jobs.id = j.id RETURNING jobs.id",
                        &[&w],
                    )
                    .await
                }
                .unwrap();
                if rows.is_empty() {
                    break;
                }
                got.extend(rows.iter().map(|r| (r.get::<_, i32>(0), w)));
            }
            got
        }));
    }
    let mut taken: HashMap<i32, i32> = HashMap::new();
    for task in tasks {
        for (id, w) in task.await.unwrap() {
            assert!(taken.insert(id, w).is_none(), "job {id} taken twice");
        }
    }
    assert_eq!(taken.len(), 1500, "every job taken");
    for r in c.query("SELECT id, worker FROM jobs", &[]).await.unwrap() {
        assert_eq!(taken.get(&r.get::<_, i32>(0)), Some(&r.get::<_, i32>(1)), "the worker that took it");
    }
}

/// Upsert storm: every session increments a few hot keys; no increment is lost.
async fn upsert_storm(t: Target) {
    let c = t.connect().await;
    c.batch_execute("DROP TABLE IF EXISTS counters; CREATE TABLE counters (k int PRIMARY KEY, n bigint NOT NULL)")
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for w in 0..SESSIONS {
        let t = t.clone();
        tasks.push(tokio::spawn(async move {
            let c = t.connect().await;
            let mut rng = Lcg(w as u64 + 1);
            let mut mine = HashMap::<i32, i64>::new();
            for _ in 0..150 {
                let k = rng.next(8) as i32;
                c.execute("INSERT INTO counters VALUES ($1, 1) ON CONFLICT (k) DO UPDATE SET n = counters.n + 1", &[&k])
                    .await
                    .unwrap();
                *mine.entry(k).or_default() += 1;
            }
            mine
        }));
    }
    let mut want = HashMap::<i32, i64>::new();
    for task in tasks {
        for (k, n) in task.await.unwrap() {
            *want.entry(k).or_default() += n;
        }
    }
    let got: HashMap<i32, i64> =
        c.query("SELECT k, n FROM counters", &[]).await.unwrap().iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, want);
}

/// ON CONFLICT DO NOTHING with an exclusion constraint as the arbiter, from many sessions
/// at once: speculative insertions never deadlock (one of two conflicting ones starts over),
/// and what went in doesn't overlap.
async fn exclusion_on_conflict(t: Target) {
    let c = t.connect().await;
    c.batch_execute("DROP TABLE IF EXISTS booking; CREATE TABLE booking (r int4range, by int, EXCLUDE USING gist (r WITH &&))")
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for w in 0..SESSIONS as i32 {
        let t = t.clone();
        tasks.push(tokio::spawn(async move {
            let c = t.connect().await;
            let mut rng = Lcg(w as u64 + 100);
            let mut inserted = 0u64;
            for _ in 0..60 {
                let lo = rng.next(400) as i32;
                let len = 1 + rng.next(6) as i32;
                inserted += c
                    .execute(
                        "INSERT INTO booking VALUES (int4range($1, $2), $3) ON CONFLICT DO NOTHING",
                        &[&lo, &(lo + len), &w],
                    )
                    .await
                    .unwrap_or_else(|e| panic!("session {w}: {e:?}"));
            }
            inserted
        }));
    }
    let mut inserted = 0;
    for task in tasks {
        inserted += task.await.unwrap();
    }
    let rows: i64 = c.query_one("SELECT count(*) FROM booking", &[]).await.unwrap().get(0);
    assert_eq!(rows as u64, inserted);
    let overlaps: i64 = c
        .query_one("SELECT count(*) FROM booking a JOIN booking b ON a.ctid < b.ctid AND a.r && b.r", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(overlaps, 0);
}

/// SERIALIZABLE transfers retried on serialization failures (and deadlocks): money is
/// neither made nor lost, every transfer eventually commits once.
async fn serializable_transfers(t: Target) {
    let c = t.connect().await;
    c.batch_execute(
        "DROP TABLE IF EXISTS accounts, ledger;
         CREATE TABLE accounts (id int PRIMARY KEY, balance bigint NOT NULL);
         CREATE TABLE ledger (src int, dst int, amount bigint);
         INSERT INTO accounts SELECT g, 1000 FROM generate_series(1, 6) g",
    )
    .await
    .unwrap();
    let retries = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for w in 0..SESSIONS {
        let (t, retries) = (t.clone(), retries.clone());
        tasks.push(tokio::spawn(async move {
            let c = t.connect().await;
            let mut rng = Lcg(w as u64 + 7);
            for _ in 0..25 {
                let src = 1 + rng.next(6) as i32;
                let dst = 1 + (src as u64 % 6 + rng.next(5)) as i32 % 6;
                let amount = 1 + rng.next(20) as i64;
                loop {
                    let r = c
                        .batch_execute(&format!(
                            "BEGIN ISOLATION LEVEL SERIALIZABLE;
                             SELECT sum(balance) FROM accounts;
                             UPDATE accounts SET balance = balance - {amount} WHERE id = {src};
                             UPDATE accounts SET balance = balance + {amount} WHERE id = {dst};
                             INSERT INTO ledger VALUES ({src}, {dst}, {amount});
                             COMMIT"
                        ))
                        .await;
                    match r {
                        Ok(()) => break,
                        Err(e) if matches!(e.code(), Some(c) if *c == SqlState::T_R_SERIALIZATION_FAILURE || *c == SqlState::T_R_DEADLOCK_DETECTED) => {
                            c.batch_execute("ROLLBACK").await.unwrap();
                            retries.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => panic!("transfer: {e:?}"),
                    }
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let (total, n): (i64, i64) = {
        let r = c
            .query_one("SELECT (SELECT sum(balance) FROM accounts)::int8, (SELECT count(*) FROM ledger)", &[])
            .await
            .unwrap();
        (r.get(0), r.get(1))
    };
    assert_eq!((total, n), (6000, SESSIONS as i64 * 25));
    let wrong: i64 = c
        .query_one(
            "SELECT count(*) FROM accounts a WHERE balance <> 1000
               + coalesce((SELECT sum(amount) FROM ledger WHERE dst = a.id), 0)
               - coalesce((SELECT sum(amount) FROM ledger WHERE src = a.id), 0)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(wrong, 0, "balances follow the ledger");
    assert!(retries.load(Ordering::Relaxed) > 0, "contended enough to conflict");
}

/// Leader election by pg_try_advisory_xact_lock: at most one leader at a time.
async fn advisory_leader(t: Target) {
    let inside = Arc::new(AtomicUsize::new(0));
    let terms = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..SESSIONS {
        let (t, inside, terms) = (t.clone(), inside.clone(), terms.clone());
        tasks.push(tokio::spawn(async move {
            let c = t.connect().await;
            for _ in 0..40 {
                c.batch_execute("BEGIN").await.unwrap();
                let leader: bool = c.query_one("SELECT pg_try_advisory_xact_lock(42)", &[]).await.unwrap().get(0);
                if leader {
                    assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two leaders at once");
                    terms.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                }
                c.batch_execute("COMMIT").await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert!(terms.load(Ordering::Relaxed) > 0);
}

/// LISTEN / NOTIFY: a transaction's notifications arrive together, in the order sent;
/// transactions' in commit order (each sender's in its order); none lost or doubled.
async fn notify_order(t: Target) {
    let (listener, mut conn) = t.0.connect(NoTls).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut messages = futures::stream::poll_fn(move |cx| conn.poll_message(cx));
        while let Some(Ok(m)) = messages.next().await {
            if let AsyncMessage::Notification(n) = m {
                let _ = tx.send(n.payload().to_string());
            }
        }
    });
    listener.batch_execute("LISTEN appconc").await.unwrap();
    const TXNS: usize = 25;
    let mut tasks = Vec::new();
    for w in 0..SESSIONS {
        let t = t.clone();
        tasks.push(tokio::spawn(async move {
            let c = t.connect().await;
            for i in 0..TXNS {
                c.batch_execute(&format!(
                    "BEGIN; NOTIFY appconc, '{w}:{i}:a'; SELECT pg_notify('appconc', '{w}:{i}:b'); NOTIFY appconc, '{w}:{i}:c'; COMMIT"
                ))
                .await
                .unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let mut got = Vec::new();
    while got.len() < SESSIONS * TXNS * 3 {
        let p = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("all notifications").unwrap();
        got.push(p);
    }
    let mut last: HashMap<usize, i64> = HashMap::new();
    let mut seen = HashSet::new();
    for chunk in got.chunks(3) {
        let parts: Vec<Vec<&str>> = chunk.iter().map(|p| p.split(':').collect()).collect();
        let (w, i) = (parts[0][0].parse::<usize>().unwrap(), parts[0][1].parse::<i64>().unwrap());
        assert!(
            parts.iter().zip(["a", "b", "c"]).all(|(p, s)| p[0] == parts[0][0] && p[1] == parts[0][1] && p[2] == s),
            "one transaction's notifications together, in order: {chunk:?}"
        );
        assert!(*last.get(&w).unwrap_or(&-1) < i, "sender {w} in commit order");
        last.insert(w, i);
        assert!(seen.insert((w, i)));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err(), "nothing more");
}

#[test]
fn job_queue_skip_locked() {
    on_engine(job_queue);
}

#[test]
fn upsert_storm_loses_nothing() {
    on_engine(upsert_storm);
}

#[test]
fn exclusion_on_conflict_never_deadlocks() {
    on_engine(exclusion_on_conflict);
}

#[test]
fn serializable_transfers_retried() {
    on_engine(serializable_transfers);
}

#[test]
fn advisory_lock_leader_election() {
    on_engine(advisory_leader);
}

#[test]
fn notifications_in_commit_order() {
    on_engine(notify_order);
}

/// The same checks against a reference PostgreSQL (`APPCONC_REF`, `APPCONC_PASSWORD`): it
/// makes a database `appconc` there and drops it at the end.
#[test]
#[ignore]
fn against_reference() {
    let Ok(addr) = std::env::var("APPCONC_REF") else {
        eprintln!("APPCONC_REF not set");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let mut c = Config::new();
    c.host(host).port(port.parse().unwrap()).user("postgres").dbname("postgres");
    if let Ok(pw) = std::env::var("APPCONC_PASSWORD") {
        c.password(pw);
    }
    runtime().block_on(async {
        let admin = Target(c.clone()).connect().await;
        admin.batch_execute("DROP DATABASE IF EXISTS appconc").await.unwrap();
        admin.batch_execute("CREATE DATABASE appconc").await.unwrap();
        let mut t = c.clone();
        t.dbname("appconc");
        let t = Target(t);
        job_queue(t.clone()).await;
        upsert_storm(t.clone()).await;
        exclusion_on_conflict(t.clone()).await;
        serializable_transfers(t.clone()).await;
        advisory_leader(t.clone()).await;
        notify_order(t).await;
        admin.batch_execute("DROP DATABASE appconc WITH (FORCE)").await.unwrap();
    });
}
