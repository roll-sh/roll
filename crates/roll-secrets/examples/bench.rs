//! How long the hot paths take with a large environment, on SQLite or (with
//! `ROLL_CODE_TEST_DATABASE=postgres://…`) the system database's dialect:
//! `cargo run -p roll-secrets --release --example bench -- 9000`.
//!
//! `… --example bench -- concurrent 1,10,50,200,500 [secs] [nodes]` (Postgres only): that many
//! resolvers, spread over `nodes` (each its own pool), reading one project's values at once, each read audited onto the project's chain, as a rollout
//! or a CI matrix does, while one writer changes a key every 20 ms. A line per count: p50, p99,
//! reads a second, serialization retries (the database's rolled-back transactions; reads run
//! outside transactions), errors (failed reads, and reads whose audit row was never written), and
//! the writer's changes, p99 and failures.
//!
//! `… --example bench -- big 1000,9900`: one change of that many new keys (an `.env` import, a
//! move-in), then one change of the same keys' new values (a re-import, a promote), each into a
//! fresh project. A line per change: wall time and, on Postgres with `pg_stat_statements` loaded,
//! the statements it ran and the three that took longest.
//!
//! `… --example bench -- stampede 100,1000 [keys] [workers] [pool]`: that many identities (each
//! its own principal, as pods' workload logins are) watching one folder of `keys` keys (9,000 plus
//! 900 references by default), each holding its digest, then one key's change. Two lines per
//! count, to the point every identity has the new digest and values: its conditional read
//! (`If-None-Match`, now a miss: the first digest after the change), then its read (`digest=1`,
//! as `roll run --watch` asks). `workers` requests at once (64), and on Postgres a pool of `pool`
//! connections (4, rolld's for secrets). Each line: wall seconds, this process's CPU seconds and,
//! with `BENCH_DB_CGROUP` (the database container's cgroup directory), the database's; CPU
//! milliseconds per identity; errors (a read that failed, or answered the old state).

use roll_secrets::{Batch, Caller, Keyring, Op, Options, Secrets};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("stampede") {
        let arg = |i: usize, or: usize| std::env::args().nth(i).and_then(|a| a.parse().ok()).unwrap_or(or);
        let ns: Vec<usize> = std::env::args().nth(2).unwrap_or("100,1000".into()).split(',').map(|n| n.parse().unwrap()).collect();
        return stampede(&ns, arg(3, 9000), arg(4, 64), arg(5, 4));
    }
    if std::env::args().nth(1).as_deref() == Some("concurrent") {
        let ns: Vec<usize> = std::env::args().nth(2).unwrap_or("1,10,50,200,500".into()).split(',').map(|n| n.parse().unwrap()).collect();
        let secs: u64 = std::env::args().nth(3).and_then(|a| a.parse().ok()).unwrap_or(15);
        let nodes: usize = std::env::args().nth(4).and_then(|a| a.parse().ok()).unwrap_or(1);
        return concurrent(&ns, secs, nodes);
    }
    if std::env::args().nth(1).as_deref() == Some("big") {
        let ns: Vec<usize> = std::env::args().nth(2).unwrap_or("1000,9900".into()).split(',').map(|n| n.parse().unwrap()).collect();
        return big(&ns);
    }
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2000);
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "bench", "", None).unwrap().id;
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
    let backend = format!("{:?}", db.backend());
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), deliver: None, open: true, trust_forwarded_for: false, web_url: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    let s = Secrets::new(db, ws, opts).unwrap();
    let c = Caller::operator();
    let time = |what: &str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        println!("{backend:>8} {n:>6} keys  {what:<24} {:>8.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    };
    let ops: Vec<Op> = (0..n)
        .map(|i| Op::Set { key: format!("K{i:05}"), value: Some(format!("value-{i:05}-abcdefghijklmnop")), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None })
        .chain((0..n / 10).map(|i| Op::Set { key: format!("R{i:05}"), value: Some(format!("${{K{i:05}}}/x")), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }))
        .collect();
    time("one change (all keys)", &mut || {
        s.batch(&project, "production", Batch { path: None, message: None, ops: ops.clone(), if_seq: None }, &c).unwrap();
    });
    time("one key", &mut || {
        s.batch(&project, "production", Batch { path: None, message: None, ops: vec![Op::Set { key: "ONE".into(), value: Some("1".into()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }], if_seq: None }, &c).unwrap();
    });
    for _ in 0..2 {
        time("grid", &mut || drop(s.grid(&project, "/", &c).unwrap()));
        time("list", &mut || drop(s.secrets(&project, "production", "/", &c).unwrap()));
        time("values (resolved)", &mut || drop(s.values(&project, "production", "/", true, None, &c).unwrap()));
        time("digest", &mut || drop(s.digest(&project, "production", "/", &c).unwrap()));
        time("insights", &mut || drop(s.insights(&project, "production", "/", &c).unwrap()));
        time("health", &mut || drop(s.project_health(&project, &c, false).unwrap()));
        time("compare", &mut || drop(s.compare(&project, "production", "staging", "/", &c).unwrap()));
    }
    // What a CSI driver, an operator or `roll run --watch` polls: a conditional read that stays a
    // 304, and the digest; per call, over 100 calls.
    let etag = s.digest(&project, "production", "/", &c).unwrap();
    let polls = |what: &str, f: &mut dyn FnMut()| {
        let mut ms: Vec<f64> = (0..100)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!("{backend:>8} {n:>6} keys  {what:<24} {:>8.1} ms median {:>8.1} ms p95", ms[50], ms[95]);
    };
    for _ in 0..2 {
        polls("304 (If-None-Match)", &mut || assert!(s.unchanged(&project, "production", "/", &etag, true, &c).unwrap().is_some()));
        polls("digest x100", &mut || drop(s.digest(&project, "production", "/", &c).unwrap()));
    }
    // The first digest after a change (one key written, untimed, before each).
    let mut ms: Vec<f64> = (0..20)
        .map(|i| {
            s.batch(&project, "production", Batch { path: None, message: None, ops: vec![Op::Set { key: "ONE".into(), value: Some(i.to_string()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }], if_seq: None }, &c).unwrap();
            let t = Instant::now();
            drop(s.digest(&project, "production", "/", &c).unwrap());
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    println!("{backend:>8} {n:>6} keys  {:<24} {:>8.1} ms median {:>8.1} ms p95", "digest after a write", ms[10], ms[19]);
    // Reach's walk at its cap: 300 folders import production (it stops at 256), each read once.
    let from = vec![roll_secrets::Import { environment: "production".into(), path: "/".into(), project: None }];
    for i in 0..300 {
        let at = format!("/f{i:03}");
        s.set_imports(&project, "staging", &at, from.clone(), &c).unwrap();
        drop(s.values(&project, "staging", &at, true, None, &c).unwrap());
    }
    for _ in 0..2 {
        time("reach (300 importers)", &mut || drop(s.reach(&project, "production", "/", None, &c).unwrap()));
    }
    // A sandbox's outgoing request swapping a placeholder (docs/secrets.md, "Using a key without
    // seeing it"): what roll's egress proxy calls per request, over 1,000 calls.
    use roll_secrets::proxy::{Place, Position};
    let hosts = roll_secrets::placeholders::Hosts { hosts: vec!["api.example.com".into()], header: Some("Authorization: Bearer {}".into()), query: None, untrusted: false };
    s.set_hosts(&project, "production", "/", "K00001", hosts, &c).unwrap();
    let sandbox = s.untrusted(&project, &Caller::consumer("sandbox:bench")).unwrap();
    let ask = roll_secrets::placeholders::Ask { minutes: None, maker: Some("operator") };
    let v = s.values_with_placeholders(&project, "production", "/", "sandbox:bench", false, ask, &sandbox).unwrap();
    let ph = v.values["K00001"].expose().to_string();
    let bearer = format!("Bearer {ph}");
    for _ in 0..2 {
        let mut ms: Vec<f64> = (0..1000)
            .map(|_| {
                let t = Instant::now();
                drop(s.substitute(&ph, "api.example.com", Position { place: Place::Header, name: "authorization", value: &bearer }).unwrap());
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!("{backend:>8} {n:>6} keys  {:<24} {:>8.2} ms p50 {:>8.2} ms p99", "substitute", ms[500], ms[990]);
    }
}

fn concurrent(ns: &[usize], secs: u64, nodes: usize) {
    use std::time::Duration;
    let url = std::env::var(code_sql::TEST_DATABASE_ENV).expect("concurrent runs on Postgres: set ROLL_CODE_TEST_DATABASE=postgres://…");
    let most = ns.iter().copied().max().unwrap_or(1);
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open(&code_sql::Target::Sqlite(dir.path().join("workspace.db"))).unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "bench", "", None).unwrap().id;
    // A connection per resolver, the nodes' pools together (under Postgres' 500, with room for its
    // own).
    let schema = format!("bench_chain_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let pool = most.min(490).div_ceil(nodes);
    let connect = || code_sql::Db::connect(&url, &schema, code_sql::PgOptions { max_connections: pool, ..Default::default() }).unwrap();
    let probe = connect();
    let opts = || Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), deliver: None, open: true, trust_forwarded_for: false, web_url: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    let all: Vec<Arc<Secrets>> = (0..nodes).map(|_| Arc::new(Secrets::new(connect(), ws.clone(), opts()).unwrap())).collect();
    let (s, c) = (&all[0], Caller::operator());
    let ops = (0..20).map(|i| Op::Set { key: format!("APP_{i:02}"), value: Some(format!("value-{i:02}-abcdefghijklmnop")), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }).collect();
    s.batch(&project, "production", Batch { path: None, message: None, ops, if_seq: None }, &c).unwrap();
    // Every resolver reads the project's production values until `stop`, and the writer writes:
    // (read latencies in ms, read errors, seconds, write latencies, write errors).
    type Timed = (Vec<f64>, usize);
    let run = |n: usize, for_: Duration| -> (Vec<f64>, usize, f64, Vec<f64>, usize) {
        let stop = Instant::now() + for_;
        let started = Instant::now();
        let (out, (wms, werrors)): (Vec<Timed>, Timed) = std::thread::scope(|sc| {
            let writer = sc.spawn(|| {
                let (mut ms, mut errors) = (vec![], 0);
                for i in 0.. {
                    if Instant::now() >= stop {
                        break;
                    }
                    let t = Instant::now();
                    let ops = vec![Op::Set { key: "ROLLOUT".into(), value: Some(i.to_string()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }];
                    if s.batch(&project, "production", Batch { path: None, message: None, ops, if_seq: None }, &c).is_err() {
                        errors += 1;
                    }
                    ms.push(t.elapsed().as_secs_f64() * 1000.0);
                    std::thread::sleep(Duration::from_millis(20));
                }
                (ms, errors)
            });
            let hs: Vec<_> = (0..n)
                .map(|i| {
                    let (s, project, c) = (&all[i % nodes], &project, &c);
                    sc.spawn(move || {
                        let (mut ms, mut errors) = (vec![], 0);
                        while Instant::now() < stop {
                            let t = Instant::now();
                            match s.values(project, "production", "/", true, None, c) {
                                Ok(v) => drop(v),
                                Err(_) => errors += 1,
                            }
                            ms.push(t.elapsed().as_secs_f64() * 1000.0);
                        }
                        (ms, errors)
                    })
                })
                .collect();
            (hs.into_iter().map(|h| h.join().unwrap()).collect(), writer.join().unwrap())
        });
        let elapsed = started.elapsed().as_secs_f64();
        let errors = out.iter().map(|o| o.1).sum();
        (out.into_iter().flat_map(|o| o.0).collect(), errors, elapsed, wms, werrors)
    };
    // Postgres flushes an idle connection's counters within 10 s.
    let counters = || {
        std::thread::sleep(Duration::from_secs(11));
        let rollbacks: i64 = probe.read(|c| c.query_row("SELECT xact_rollback FROM pg_stat_database WHERE datname = current_database()", (), |r| r.get(0))).unwrap();
        let rows: i64 = probe.read(|c| c.query_row("SELECT COUNT(*) FROM audit WHERE project_id = ? AND action = 'secrets.export'", [&project], |r| r.get(0))).unwrap();
        (rollbacks, rows)
    };
    let load = || std::fs::read_to_string("/proc/loadavg").unwrap_or_default().split_whitespace().next().unwrap_or("?").to_string();
    run(most, Duration::from_secs(3));
    let mut before = counters();
    println!("resolvers\treads\tper_s\tp50_ms\tp99_ms\tretries\terrors\tunaudited\twrites\twrite_p99_ms\twrite_errors\tload_start\tload_end");
    for &n in ns {
        let load_start = load();
        let (mut ms, errors, elapsed, mut wms, werrors) = run(n, Duration::from_secs(secs));
        let load_end = load();
        let after = counters();
        ms.sort_by(f64::total_cmp);
        wms.sort_by(f64::total_cmp);
        let at = |v: &[f64], q: f64| v[((v.len() as f64 * q) as usize).min(v.len() - 1)];
        let reads = ms.len() as i64;
        let unaudited = reads - errors as i64 - (after.1 - before.1);
        println!(
            "{n}\t{reads}\t{:.0}\t{:.2}\t{:.2}\t{}\t{errors}\t{unaudited}\t{}\t{:.2}\t{werrors}\t{load_start}\t{load_end}",
            reads as f64 / elapsed,
            at(&ms, 0.5),
            at(&ms, 0.99),
            after.0 - before.0,
            wms.len(),
            at(&wms, 0.99)
        );
        before = after;
    }
    probe.write(|c| c.execute_batch(&format!("DROP SCHEMA {schema} CASCADE"))).unwrap();
}

fn big(ns: &[usize]) {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open(&code_sql::Target::Sqlite(dir.path().join("workspace.db"))).unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "bench_big").unwrap();
    let backend = format!("{:?}", db.backend());
    let stats = db.backend() == code_sql::Backend::Postgres && db.read(|c| c.query_row("SELECT COUNT(*) FROM pg_extension WHERE extname = 'pg_stat_statements'", (), |r| r.get::<_, i64>(0))).unwrap_or(0) > 0;
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), deliver: None, open: true, trust_forwarded_for: false, web_url: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    let s = Secrets::new(db.clone(), ws.clone(), opts).unwrap();
    let c = Caller::operator();
    let load = || std::fs::read_to_string("/proc/loadavg").unwrap_or_default().split_whitespace().next().unwrap_or("?").to_string();
    for &n in ns {
        let project = ws.create("", &format!("big-{n}"), "", None).unwrap().id;
        for (what, round) in [("new keys", 0), ("new values", 1)] {
            let ops: Vec<Op> = (0..n)
                .map(|i| Op::Set { key: format!("K{i:05}"), value: Some(format!("value-{round}-{i:05}-abcdefghijklmnop")), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None })
                .collect();
            if stats {
                db.read(|c| c.query_row("SELECT 1 FROM public.pg_stat_statements_reset()", (), |_| Ok(()))).unwrap();
            }
            let load_start = load();
            let t = Instant::now();
            let a = s.batch(&project, "production", Batch { path: None, message: None, ops, if_seq: None }, &c).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(a.items.len(), n);
            let mut line = format!("{backend:>8} {n:>6} keys  one change, {what:<10} {ms:>9.1} ms  load {load_start}");
            if stats {
                let (calls, top) = db
                    .read(|c| {
                        let calls: i64 = c.query_row("SELECT COALESCE(SUM(calls), 0)::bigint FROM public.pg_stat_statements WHERE query NOT LIKE '%pg_stat_statements%'", (), |r| r.get(0))?;
                        let top: Vec<(String, i64, f64)> = c.query(
                            "SELECT query, calls, total_exec_time FROM public.pg_stat_statements WHERE query NOT LIKE '%pg_stat_statements%' ORDER BY total_exec_time DESC LIMIT 3",
                            (),
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )?;
                        Ok::<_, code_sql::Error>((calls, top))
                    })
                    .unwrap();
                line += &format!("  {calls} statements");
                for (q, calls, ms) in top {
                    let q: String = q.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(70).collect();
                    line += &format!("\n{:>40} {calls:>6} x  {ms:>8.1} ms  {q}", "");
                }
            }
            println!("{line}");
        }
    }
}

fn stampede(ns: &[usize], n: usize, workers: usize, pool: usize) {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open(&code_sql::Target::Sqlite(dir.path().join("workspace.db"))).unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "bench", "", None).unwrap().id;
    let (db, schema) = match std::env::var(code_sql::TEST_DATABASE_ENV) {
        Ok(url) if !url.is_empty() => {
            let schema = format!("bench_stampede_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
            (code_sql::Db::connect(&url, &schema, code_sql::PgOptions { max_connections: pool, ..Default::default() }).unwrap(), Some(schema))
        }
        _ => (code_sql::Db::open(&code_sql::Target::Sqlite(dir.path().join("secrets.db"))).unwrap(), None),
    };
    let backend = format!("{:?}", db.backend());
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), deliver: None, open: true, trust_forwarded_for: false, web_url: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    let s = Secrets::new(db.clone(), ws, opts).unwrap();
    let c = Caller::operator();
    let set = |key: String, value: String| Op::Set { key, value: Some(value), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None };
    let ops = (0..n).map(|i| set(format!("K{i:05}"), format!("value-{i:05}-abcdefghijklmnop"))).chain((0..n / 10).map(|i| set(format!("R{i:05}"), format!("${{K{i:05}}}/x")))).collect();
    s.batch(&project, "production", Batch { path: None, message: None, ops, if_seq: None }, &c).unwrap();
    // This process's CPU seconds (user and system), and the database container's.
    let cpu = || {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let f: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
        (f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap()) / 100.0
    };
    let db_cpu = || {
        let dir = std::env::var("BENCH_DB_CGROUP").ok()?;
        let stat = std::fs::read_to_string(format!("{dir}/cpu.stat")).ok()?;
        stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.parse::<f64>().ok()).map(|us| us / 1e6)
    };
    let load = || std::fs::read_to_string("/proc/loadavg").unwrap_or_default().split_whitespace().next().unwrap_or("?").to_string();
    // One identity's request: whether it answered what it should.
    type Poll<'a> = &'a (dyn Fn(&Caller) -> bool + Sync);
    // `f` for every identity, `workers` at once, as rolld's blocking pool runs requests: the
    // number that failed.
    let each = |pods: &[Caller], f: Poll<'_>| -> usize {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failed = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for _ in 0..workers.min(pods.len()) {
                sc.spawn(|| {
                    while let Some(pod) = pods.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        if !f(pod) {
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        failed.into_inner()
    };
    println!("backend\tidentities\tkeys\tstep\twall_s\tcpu_s\tdb_cpu_s\tcpu_ms_per_identity\terrors\tload");
    for (round, &count) in ns.iter().enumerate() {
        // New identities each round, so none starts with a digest an earlier round left.
        let pods: Vec<Caller> = (0..count).map(|i| Caller::consumer(&format!("workload:r{round}-pod{i:04}"))).collect();
        let before = s.digest(&project, "production", "/", &pods[0]).unwrap();
        each(&pods, &|p| s.digest(&project, "production", "/", p).is_ok_and(|d| d == before));
        let value = format!("rotated-{round}");
        s.batch(&project, "production", Batch { path: None, message: None, ops: vec![set("K00000".into(), value.clone())], if_seq: None }, &c).unwrap();
        let steps: [(&str, Poll<'_>); 2] = [
            ("conditional read (miss)", &|p| matches!(s.unchanged(&project, "production", "/", &before, true, p), Ok(None))),
            ("read (digest=1)", &|p| {
                let digest = s.digest(&project, "production", "/", p);
                let v = s.values_for(&project, "production", "/", true, None, true, p);
                digest.is_ok_and(|d| d != before) && v.is_ok_and(|v| v.values.len() == n + n / 10 && v.values["K00000"].expose() == value)
            }),
        ];
        for (step, f) in steps {
            let (load, cpu0, db0, t) = (load(), cpu(), db_cpu(), Instant::now());
            let errors = each(&pods, f);
            let (wall, used, db) = (t.elapsed().as_secs_f64(), cpu() - cpu0, db_cpu().zip(db0).map(|(a, b)| a - b));
            let db_s = db.map(|d| format!("{d:.1}")).unwrap_or("-".into());
            let per = (used + db.unwrap_or(0.0)) * 1000.0 / count as f64;
            println!("{backend}\t{count}\t{}\t{step}\t{wall:.1}\t{used:.1}\t{db_s}\t{per:.0}\t{errors}\t{load}", n + n / 10);
        }
    }
    if let Some(schema) = schema {
        db.write(|c| c.execute_batch(&format!("DROP SCHEMA {schema} CASCADE"))).unwrap();
    }
}
