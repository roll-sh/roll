//! pg-engine benchmarks: the same workloads against the engine over the wire, the engine
//! in-process (how roll embeds it) and PostgreSQL. See `tools/bench/run.sh` and BENCH.md.
//!
//!     bench --target wire --port 5433 <workload> [options]
//!     bench --target inproc --data <dir> <workload> [options]
//!
//! Workloads: oltp-load, oltp, analytic-load, analytic, tpch-load, tpch, bulk, ddl, scale, idle,
//! startup, startup-catalog. Results are printed as `RESULT\t<label>\t<workload>\t<case>\t<k=v>...` lines.

mod tpch;

use bytes::BytesMut;
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::session::{Outcome, Session};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_postgres::types::{IsNull, ToSql, Type};

type Res<T> = std::result::Result<T, String>;

// The allocator the roll-engine server has (the in-process numbers are with it).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ---------------------------------------------------------------------------------------------
// Connections

trait Conn {
    /// Run a simple query; (rows, a hash of the text of every row).
    fn simple(&mut self, sql: &str) -> Res<(u64, u64)>;
    /// Prepare `sql` with int4 parameters; returns its handle.
    fn prepare(&mut self, sql: &str, nparams: usize) -> Res<usize>;
    /// Run a prepared statement with int parameters (extended protocol); rows returned.
    fn exec(&mut self, id: usize, params: &[i64]) -> Res<u64>;
    fn copy_in(&mut self, sql: &str, data: &[u8]) -> Res<()>;
    /// Bytes of COPY TO STDOUT data.
    fn copy_out(&mut self, sql: &str) -> Res<u64>;
}

#[derive(Debug)]
struct IntParam(i64);

impl ToSql for IntParam {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match *ty {
            Type::INT2 => (self.0 as i16).to_sql(ty, out),
            Type::INT8 => self.0.to_sql(ty, out),
            _ => (self.0 as i32).to_sql(ty, out),
        }
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    fn to_sql_checked(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.to_sql(ty, out)
    }
}

struct Wire {
    rt: tokio::runtime::Runtime,
    c: tokio_postgres::Client,
    stmts: Vec<tokio_postgres::Statement>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn db_err(e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => format!("{}: {}", d.code().code(), d.message()),
        None => e.to_string(),
    }
}

impl Wire {
    fn connect(host: &str, port: u16, password: &str, db: &str) -> Res<Wire> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(err)?;
        let cfg = format!(
            "host={host} port={port} user=postgres password={password} dbname={db} application_name=bench"
        );
        let (c, conn) = rt
            .block_on(tokio_postgres::connect(&cfg, tokio_postgres::NoTls))
            .map_err(db_err)?;
        rt.spawn(async move {
            let _ = conn.await;
        });
        Ok(Wire {
            rt,
            c,
            stmts: Vec::new(),
        })
    }
}

impl Conn for Wire {
    fn simple(&mut self, sql: &str) -> Res<(u64, u64)> {
        let msgs = self.rt.block_on(self.c.simple_query(sql)).map_err(db_err)?;
        let mut rows = 0;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for m in msgs {
            if let tokio_postgres::SimpleQueryMessage::Row(r) = m {
                rows += 1;
                for i in 0..r.len() {
                    r.get(i).hash(&mut h);
                }
            }
        }
        Ok((rows, h.finish()))
    }
    fn prepare(&mut self, sql: &str, nparams: usize) -> Res<usize> {
        let types = vec![Type::INT4; nparams];
        let s = self
            .rt
            .block_on(self.c.prepare_typed(sql, &types))
            .map_err(db_err)?;
        self.stmts.push(s);
        Ok(self.stmts.len() - 1)
    }
    fn exec(&mut self, id: usize, params: &[i64]) -> Res<u64> {
        let ps: Vec<IntParam> = params.iter().map(|p| IntParam(*p)).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = ps.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let st = &self.stmts[id];
        if st.columns().is_empty() {
            self.rt.block_on(self.c.execute(st, &refs)).map_err(db_err)
        } else {
            let rows = self.rt.block_on(self.c.query(st, &refs)).map_err(db_err)?;
            Ok(rows.len() as u64)
        }
    }
    fn copy_in(&mut self, sql: &str, data: &[u8]) -> Res<()> {
        use futures::SinkExt;
        self.rt.block_on(async {
            let sink = self.c.copy_in::<_, bytes::Bytes>(sql).await.map_err(db_err)?;
            futures::pin_mut!(sink);
            for chunk in data.chunks(1 << 16) {
                sink.send(bytes::Bytes::copy_from_slice(chunk))
                    .await
                    .map_err(db_err)?;
            }
            sink.finish().await.map_err(db_err)?;
            Ok(())
        })
    }
    fn copy_out(&mut self, sql: &str) -> Res<u64> {
        use futures::StreamExt;
        self.rt.block_on(async {
            let stream = self.c.copy_out(sql).await.map_err(db_err)?;
            futures::pin_mut!(stream);
            let mut n = 0u64;
            while let Some(b) = stream.next().await {
                n += b.map_err(db_err)?.len() as u64;
            }
            Ok(n)
        })
    }
}

struct InProc {
    s: Session,
    names: Vec<String>,
}

fn pg_err(e: pg_engine::error::Error) -> String {
    format!("{}: {}", e.code, e.message)
}

impl Conn for InProc {
    fn simple(&mut self, sql: &str) -> Res<(u64, u64)> {
        let rs = self.s.query(sql).map_err(pg_err)?;
        Ok((rs.iter().map(|r| r.rows.len() as u64).sum(), 0))
    }
    fn prepare(&mut self, sql: &str, nparams: usize) -> Res<usize> {
        let name = format!("b{}", self.names.len());
        self.s
            .parse_message(&name, sql, &vec![23; nparams])
            .map_err(pg_err)?;
        self.s.sync().map_err(pg_err)?;
        self.names.push(name);
        Ok(self.names.len() - 1)
    }
    fn exec(&mut self, id: usize, params: &[i64]) -> Res<u64> {
        let ps = params
            .iter()
            .map(|p| Some(p.to_string().into_bytes()))
            .collect();
        let name = &self.names[id];
        let r = self
            .s
            .bind_message("", name, ps, &[0], Vec::new())
            .and_then(|_| self.s.execute_portal("", 0));
        let n = match r {
            Ok((Outcome::Done(r), _)) => {
                if r.cols.is_some() {
                    r.rows.len() as u64
                } else {
                    r.tag
                        .rsplit(' ')
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0)
                }
            }
            Ok(_) => 0,
            Err(e) => {
                let _ = self.s.sync();
                return Err(pg_err(e));
            }
        };
        self.s.sync().map_err(pg_err)?;
        Ok(n)
    }
    fn copy_in(&mut self, sql: &str, data: &[u8]) -> Res<()> {
        self.s.query_copy(sql, data).map_err(pg_err)?;
        Ok(())
    }
    fn copy_out(&mut self, sql: &str) -> Res<u64> {
        let ps = self.s.parse(sql).map_err(pg_err)?;
        let mut n = 0;
        for p in ps {
            if let Outcome::CopyOut(c) = self.s.execute(&p).map_err(pg_err)? {
                n += c.data.iter().map(|d| d.len() as u64).sum::<u64>();
            }
        }
        self.s.end_batch().map_err(pg_err)?;
        Ok(n)
    }
}

enum Target {
    Wire {
        host: String,
        port: u16,
        password: String,
    },
    InProc {
        engine: Arc<Engine>,
    },
}

impl Target {
    fn connect(&self, db: &str) -> Res<Box<dyn Conn>> {
        match self {
            Target::Wire {
                host,
                port,
                password,
            } => Ok(Box::new(Wire::connect(host, *port, password, db)?)),
            Target::InProc { engine } => {
                let (pid, _, _) = engine.register_backend(None, None);
                let s = engine
                    .connect(pid, db, "postgres", "bench")
                    .map_err(pg_err)?;
                Ok(Box::new(InProc {
                    s,
                    names: Vec::new(),
                }))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Memory

#[derive(Clone)]
enum MemProbe {
    None,
    Pid(u32),
    /// A cgroup v2 directory: anon + shmem of memory.stat (a PostgreSQL container: backends'
    /// private memory plus shared_buffers as far as it was touched).
    Cgroup(String),
}

impl MemProbe {
    fn bytes(&self) -> u64 {
        match self {
            MemProbe::None => 0,
            MemProbe::Pid(p) => std::fs::read_to_string(format!("/proc/{p}/status"))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                })
                .map(|kb| kb * 1024)
                .unwrap_or(0),
            MemProbe::Cgroup(dir) => std::fs::read_to_string(format!("{dir}/memory.stat"))
                .map(|s| {
                    s.lines()
                        .filter_map(|l| {
                            let (k, v) = l.split_once(' ')?;
                            matches!(k, "anon" | "shmem").then(|| v.parse::<u64>().ok())?
                        })
                        .sum()
                })
                .unwrap_or(0),
        }
    }
}

/// Samples memory every 50 ms while alive; `stop` returns the peak.
struct MemSampler {
    stop: Arc<AtomicBool>,
    h: Option<std::thread::JoinHandle<u64>>,
}

impl MemSampler {
    fn start(p: &MemProbe) -> MemSampler {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let p = p.clone();
        let h = std::thread::spawn(move || {
            let mut peak = 0;
            while !s2.load(Ordering::Relaxed) {
                peak = peak.max(p.bytes());
                std::thread::sleep(Duration::from_millis(50));
            }
            peak.max(p.bytes())
        });
        MemSampler { stop, h: Some(h) }
    }
    fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.h.take().unwrap().join().unwrap()
    }
}

fn mb(b: u64) -> String {
    format!("{:.1}", b as f64 / (1 << 20) as f64)
}

// ---------------------------------------------------------------------------------------------
// Running and reporting

struct Ctx {
    target: Target,
    db: String,
    label: String,
    mem: MemProbe,
    opts: HashMap<String, String>,
}

impl Ctx {
    fn opt<T: std::str::FromStr>(&self, k: &str, d: T) -> T {
        self.opts
            .get(k)
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    }
    fn report(&self, workload: &str, case: &str, kv: &[(&str, String)]) {
        let mut line = format!("RESULT\t{}\t{workload}\t{case}", self.label);
        for (k, v) in kv {
            line.push_str(&format!("\t{k}={v}"));
        }
        println!("{line}");
    }
    fn conn(&self) -> Box<dyn Conn> {
        self.target
            .connect(&self.db)
            .unwrap_or_else(|e| panic!("connect: {e}"))
    }
}

fn pct(sorted: &[u32], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i] as f64 / 1000.0
}

/// One client operation: the connection, its prepared statements' handles, its random stream.
type Op<'a> = &'a (dyn Fn(&mut dyn Conn, &[usize], &mut tpch::Rng) -> Res<()> + Sync);

/// Run `op` on `clients` connections for `secs`, after `prep` on each; report throughput
/// and latency percentiles (ms).
fn run_clients(
    cx: &Ctx,
    workload: &str,
    case: &str,
    clients: usize,
    secs: f64,
    prep: &(dyn Fn(&mut dyn Conn) -> Vec<usize> + Sync),
    op: Op,
) {
    let start_flag = std::sync::Barrier::new(clients + 1);
    let stop = AtomicBool::new(false);
    let mut lat: Vec<u32> = Vec::new();
    let mut errors = 0u64;
    let mut first_err = String::new();
    let sampler = MemSampler::start(&cx.mem);
    let elapsed = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..clients)
            .map(|c| {
                let start_flag = &start_flag;
                let stop = &stop;
                sc.spawn(move || {
                    // (A client that can't start still meets the others at the barrier.)
                    let ready = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut conn = cx.conn();
                        let ids = prep(conn.as_mut());
                        (conn, ids)
                    }));
                    let Ok((mut conn, ids)) = ready else {
                        start_flag.wait();
                        return (Vec::new(), 1, "client failed to start".to_string());
                    };
                    let mut rng = tpch::Rng::new(99, c as u64 + 1);
                    let mut lat = Vec::with_capacity(1 << 16);
                    let mut errors = 0u64;
                    let mut first = String::new();
                    start_flag.wait();
                    while !stop.load(Ordering::Relaxed) {
                        let t = Instant::now();
                        match op(conn.as_mut(), &ids, &mut rng) {
                            Ok(()) => lat.push(t.elapsed().as_nanos().min(u32::MAX as u128 * 1000) as u64 / 1000),
                            Err(e) => {
                                errors += 1;
                                if first.is_empty() {
                                    first = e;
                                }
                            }
                        }
                    }
                    (lat, errors, first)
                })
            })
            .collect();
        start_flag.wait();
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
        for h in hs {
            let (l, e, f) = h.join().unwrap();
            lat.extend(l.into_iter().map(|x| x.min(u32::MAX as u64) as u32));
            errors += e;
            if first_err.is_empty() {
                first_err = f;
            }
        }
        t0.elapsed().as_secs_f64()
    });
    let peak = sampler.stop();
    lat.sort_unstable();
    let n = lat.len();
    let mean = lat.iter().map(|x| *x as f64).sum::<f64>() / n.max(1) as f64 / 1000.0;
    let mut kv = vec![
        ("clients", clients.to_string()),
        ("tps", format!("{:.0}", n as f64 / elapsed)),
        ("mean_ms", format!("{mean:.3}")),
        ("p50_ms", format!("{:.3}", pct(&lat, 0.50))),
        ("p95_ms", format!("{:.3}", pct(&lat, 0.95))),
        ("p99_ms", format!("{:.3}", pct(&lat, 0.99))),
        ("errors", errors.to_string()),
        ("peak_mem_mb", mb(peak)),
    ];
    if !first_err.is_empty() {
        kv.push(("first_error", first_err.replace(['\t', '\n'], " ")));
    }
    cx.report(workload, case, &kv);
}

/// Time `f` (seconds).
fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64())
}

fn must<T>(r: Res<T>, what: &str) -> T {
    r.unwrap_or_else(|e| panic!("{what}: {e}"))
}

fn run_script(c: &mut dyn Conn, sql: &str) {
    for s in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        must(c.simple(s), s);
    }
}

// ---------------------------------------------------------------------------------------------
// OLTP

fn oltp_load(cx: &Ctx) {
    let rows: i64 = cx.opt("rows", 1_000_000);
    let mut c = cx.conn();
    let (_, secs) = timed(|| {
        run_script(
            c.as_mut(),
            &format!(
                "DROP TABLE IF EXISTS accounts, events, counters;
                 CREATE TABLE accounts (id int PRIMARY KEY, owner int NOT NULL, balance bigint NOT NULL, note text, created date NOT NULL);
                 INSERT INTO accounts SELECT g, g % 50000, (g::bigint * 7919) % 100000, 'note ' || g, date '2024-01-01' + (g % 365) FROM generate_series(1, {rows}) g;
                 CREATE INDEX accounts_owner ON accounts (owner);
                 CREATE TABLE events (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, account int NOT NULL, delta int NOT NULL, at timestamptz NOT NULL DEFAULT now());
                 CREATE INDEX events_account ON events (account);
                 CREATE TABLE counters (k int PRIMARY KEY, n bigint NOT NULL);
                 VACUUM ANALYZE accounts;
                 ANALYZE events;
                 ANALYZE counters"
            ),
        )
    });
    cx.report("oltp-load", "load", &[("rows", rows.to_string()), ("secs", format!("{secs:.2}"))]);
}

fn oltp(cx: &Ctx) {
    let rows: i64 = cx.opt("rows", 1_000_000);
    let secs: f64 = cx.opt("secs", 10.0);
    let clients_list: String = cx.opt("clients", "1,8".to_string());
    let only: String = cx.opt("only", String::new());
    type Op = (&'static str, &'static [(&'static str, usize)]);
    let cases: &[Op] = &[
        ("point_select", &[("SELECT id, owner, balance, note, created FROM accounts WHERE id = $1", 1)]),
        ("point_select_simple", &[]),
        ("range_scan_100", &[("SELECT id, balance FROM accounts WHERE id BETWEEN $1 AND $1 + 99 ORDER BY id", 1)]),
        ("secondary_lookup", &[("SELECT count(*), sum(balance) FROM accounts WHERE owner = $1", 1)]),
        ("insert", &[("INSERT INTO events (account, delta) VALUES ($1, $2)", 2)]),
        ("update_indexed", &[("UPDATE accounts SET balance = balance + $2 WHERE id = $1", 2)]),
        ("upsert", &[("INSERT INTO counters (k, n) VALUES ($1, 1) ON CONFLICT (k) DO UPDATE SET n = counters.n + 1", 1)]),
        ("short_txn", &[
            ("UPDATE accounts SET balance = balance + $2 WHERE id = $1", 2),
            ("SELECT balance FROM accounts WHERE id = $1", 1),
            ("INSERT INTO events (account, delta) VALUES ($1, $2)", 2),
        ]),
        ("select_1", &[("SELECT 1", 0)]),
    ];
    for clients in clients_list.split(',').filter_map(|c| c.parse::<usize>().ok()) {
        for (name, stmts) in cases {
            if !only.is_empty() && !only.split(',').any(|o| o == *name) {
                continue;
            }
            let stmts = *stmts;
            let prep = move |c: &mut dyn Conn| -> Vec<usize> {
                stmts
                    .iter()
                    .map(|(sql, n)| must(c.prepare(sql, *n), sql))
                    .collect()
            };
            let name = *name;
            let op = move |c: &mut dyn Conn, ids: &[usize], r: &mut tpch::Rng| -> Res<()> {
                let id = r.range(1, rows);
                let d = r.range(-500, 500);
                match name {
                    "point_select_simple" => {
                        c.simple(&format!(
                            "SELECT id, owner, balance, note, created FROM accounts WHERE id = {id}"
                        ))?;
                    }
                    "range_scan_100" => {
                        c.exec(ids[0], &[r.range(1, rows - 100)])?;
                    }
                    "secondary_lookup" => {
                        c.exec(ids[0], &[r.range(0, 49_999)])?;
                    }
                    "insert" | "update_indexed" => {
                        c.exec(ids[0], &[id, d])?;
                    }
                    "upsert" => {
                        c.exec(ids[0], &[r.range(1, 100_000)])?;
                    }
                    "short_txn" => {
                        c.simple("BEGIN")?;
                        let r2 = c
                            .exec(ids[0], &[id, d])
                            .and_then(|_| c.exec(ids[1], &[id]))
                            .and_then(|_| c.exec(ids[2], &[id, d]));
                        match r2 {
                            Ok(_) => {
                                c.simple("COMMIT")?;
                            }
                            Err(e) => {
                                c.simple("ROLLBACK")?;
                                return Err(e);
                            }
                        }
                    }
                    "select_1" => {
                        c.exec(ids[0], &[])?;
                    }
                    _ => {
                        c.exec(ids[0], &[id])?;
                    }
                }
                Ok(())
            };
            run_clients(cx, "oltp", name, clients, secs, &prep, &op);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Analytic (beyond TPC-H)

fn analytic_load(cx: &Ctx) {
    let rows: i64 = cx.opt("rows", 2_000_000);
    let mut c = cx.conn();
    let (_, secs) = timed(|| {
        run_script(
            c.as_mut(),
            &format!(
                "DROP TABLE IF EXISTS fact, dim;
                 CREATE TABLE fact (id int NOT NULL, g int NOT NULL, k int NOT NULL, x float8 NOT NULL, y numeric(12,2) NOT NULL, s text NOT NULL);
                 INSERT INTO fact SELECT i, i % 1000, (i::bigint * 7919) % 100000, ((i::bigint * 2654435761) % 1000003) / 1000.0, ((i::bigint * 40503) % 100000) / 100.0, md5(i::text) FROM generate_series(1, {rows}) i;
                 CREATE INDEX fact_id ON fact (id);
                 CREATE TABLE dim (k int PRIMARY KEY, name text NOT NULL, w int NOT NULL);
                 INSERT INTO dim SELECT k, 'name ' || (k % 100), k % 7 FROM generate_series(0, 99999) k;
                 VACUUM ANALYZE fact;
                 VACUUM ANALYZE dim"
            ),
        )
    });
    cx.report("analytic-load", "load", &[("rows", rows.to_string()), ("secs", format!("{secs:.2}"))]);
}

/// Run named queries `reps` times each; report min / median seconds, rows and a result hash.
fn run_queries(cx: &Ctx, workload: &str, queries: &[(&str, &str)]) {
    let reps: usize = cx.opt("reps", 3);
    let only: String = cx.opt("only", String::new());
    let timeout: u64 = cx.opt("timeout", 600);
    let mut c = cx.conn();
    must(c.simple(&format!("SET statement_timeout = '{timeout}s'")), "timeout");
    for (name, sql) in queries {
        if !only.is_empty() && !only.split(',').any(|o| o == *name) {
            continue;
        }
        let mut times = Vec::new();
        let mut out = (0, 0);
        let mut error = String::new();
        let sampler = MemSampler::start(&cx.mem);
        for _ in 0..reps {
            let (r, s) = timed(|| c.simple(sql));
            match r {
                Ok(r) => {
                    out = r;
                    times.push(s);
                }
                Err(e) => {
                    error = e;
                    break;
                }
            }
        }
        let peak = sampler.stop();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut kv = vec![
            ("min_s", times.first().map(|t| format!("{t:.4}")).unwrap_or_default()),
            ("median_s", times.get(times.len() / 2).map(|t| format!("{t:.4}")).unwrap_or_default()),
            ("max_s", times.last().map(|t| format!("{t:.4}")).unwrap_or_default()),
            ("rows", out.0.to_string()),
            ("hash", format!("{:016x}", out.1)),
            ("peak_mem_mb", mb(peak)),
        ];
        if !error.is_empty() {
            kv.push(("error", error.replace(['\t', '\n'], " ")));
        }
        cx.report(workload, name, &kv);
    }
}

const ANALYTIC: &[(&str, &str)] = &[
    ("seq_filter", "SELECT count(*) FROM fact WHERE x > 500 AND s LIKE '%7a%'"),
    ("sort_float", "SELECT id, x FROM fact ORDER BY x, id OFFSET 1999990"),
    ("sort_text", "SELECT s FROM fact ORDER BY s OFFSET 1999990"),
    ("top_n", "SELECT id, x FROM fact ORDER BY x DESC, id LIMIT 100"),
    ("hash_agg", "SELECT g, count(*), sum(x), avg(y), min(s), max(s) FROM fact GROUP BY g ORDER BY g"),
    ("count_distinct", "SELECT count(DISTINCT k) FROM fact"),
    ("hash_join", "SELECT d.name, count(*), sum(f.x) FROM fact f JOIN dim d ON f.k = d.k GROUP BY d.name ORDER BY d.name"),
    ("window", "SELECT sum(rn), max(cs) FROM (SELECT row_number() OVER (PARTITION BY g ORDER BY id) rn, sum(x) OVER (PARTITION BY g ORDER BY id) cs FROM fact) t"),
    ("index_range", "SELECT sum(x) FROM fact WHERE id BETWEEN 1000 AND 50000"),
    ("numeric_sum", "SELECT sum(y), avg(y) FROM fact"),
];

// ---------------------------------------------------------------------------------------------
// TPC-H

fn tpch_load(cx: &Ctx) {
    let sf: f64 = cx.opt("sf", 1.0);
    let scale = tpch::Scale { sf };
    let mut c = cx.conn();
    let sampler = MemSampler::start(&cx.mem);
    let t0 = Instant::now();
    run_script(c.as_mut(), "DROP TABLE IF EXISTS lineitem, orders, customer, partsupp, part, supplier, nation, region");
    run_script(c.as_mut(), tpch::SCHEMA);
    // COPY in chunks (a client streaming a big file; the chunk bounds the engine's buffer).
    let chunk: i64 = cx.opt("chunk", 100_000);
    for t in tpch::TABLES {
        let n = scale.rows(t);
        let ts = Instant::now();
        let mut i = 0;
        while i < n {
            let to = (i + chunk).min(n);
            let mut data = String::new();
            scale.rows_text(t, i, to, &mut data);
            must(c.copy_in(&format!("COPY {t} FROM STDIN"), data.as_bytes()), t);
            i = to;
        }
        cx.report("tpch-load", &format!("copy_{t}"), &[("secs", format!("{:.2}", ts.elapsed().as_secs_f64()))]);
    }
    let copy = t0.elapsed().as_secs_f64();
    let ti = Instant::now();
    for s in tpch::INDEXES {
        must(c.simple(s), s);
    }
    let idx = ti.elapsed().as_secs_f64();
    let ta = Instant::now();
    run_script(c.as_mut(), "VACUUM ANALYZE");
    let an = ta.elapsed().as_secs_f64();
    let peak = sampler.stop();
    cx.report(
        "tpch-load",
        "total",
        &[
            ("sf", sf.to_string()),
            ("copy_s", format!("{copy:.2}")),
            ("index_s", format!("{idx:.2}")),
            ("vacuum_analyze_s", format!("{an:.2}")),
            ("peak_mem_mb", mb(peak)),
        ],
    );
}

// ---------------------------------------------------------------------------------------------
// Bulk

fn bulk(cx: &Ctx) {
    let rows: i64 = cx.opt("rows", 1_000_000);
    let reps: usize = cx.opt("reps", 3);
    let mut data = String::new();
    {
        use std::fmt::Write;
        for i in 1..=rows {
            let _ = writeln!(data, "{i}\t{}\tsome text value {}\t{}", (i * 7919) % 100_000, i % 1000, (i % 1000) as f64 / 7.0);
        }
    }
    let mut c = cx.conn();
    let steps = [
        "copy_in", "create_index", "create_unique_index", "copy_out", "insert_select",
        "update_half", "delete_quarter", "vacuum", "analyze", "count_after",
    ];
    let mut times: HashMap<&str, Vec<f64>> = HashMap::new();
    let sampler = MemSampler::start(&cx.mem);
    for _ in 0..reps {
        run_script(c.as_mut(), "DROP TABLE IF EXISTS bulk, bulk2; CREATE TABLE bulk (id int NOT NULL, a int NOT NULL, b text, c float8); CREATE TABLE bulk2 (LIKE bulk)");
        for step in steps {
            let (r, s) = timed(|| -> Res<()> {
                match step {
                    "copy_in" => c.copy_in("COPY bulk FROM STDIN", data.as_bytes()),
                    "create_index" => c.simple("CREATE INDEX bulk_a ON bulk (a)").map(drop),
                    "create_unique_index" => c.simple("CREATE UNIQUE INDEX bulk_id ON bulk (id)").map(drop),
                    "copy_out" => c.copy_out("COPY bulk TO STDOUT").map(drop),
                    "insert_select" => c.simple("INSERT INTO bulk2 SELECT * FROM bulk").map(drop),
                    "update_half" => c.simple("UPDATE bulk SET c = c + 1 WHERE id % 2 = 0").map(drop),
                    "delete_quarter" => c.simple("DELETE FROM bulk WHERE id % 4 = 1").map(drop),
                    "vacuum" => c.simple("VACUUM bulk").map(drop),
                    "analyze" => c.simple("ANALYZE bulk").map(drop),
                    "count_after" => c.simple("SELECT count(*), sum(c) FROM bulk").map(drop),
                    _ => unreachable!(),
                }
            });
            must(r, step);
            times.entry(step).or_default().push(s);
        }
    }
    run_script(c.as_mut(), "DROP TABLE IF EXISTS bulk, bulk2");
    let peak = sampler.stop();
    for step in steps {
        let mut t = times.remove(step).unwrap_or_default();
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        cx.report(
            "bulk",
            step,
            &[
                ("rows", rows.to_string()),
                ("min_s", format!("{:.3}", t[0])),
                ("median_s", format!("{:.3}", t[t.len() / 2])),
                ("max_s", format!("{:.3}", t[t.len() - 1])),
                ("peak_mem_mb", mb(peak)),
            ],
        );
    }
}

// ---------------------------------------------------------------------------------------------
// DDL and catalogs

fn ddl(cx: &Ctx) {
    let tables: usize = cx.opt("tables", 500);
    let reps: usize = cx.opt("reps", 20);
    // (only=<query names>: just those catalog queries, for profiling; setup=0: over the
    // tables a run with keep=1 left, and keep=1: leave them.)
    let only: String = cx.opt("only", String::new());
    let setup: usize = cx.opt("setup", 1);
    let keep: usize = cx.opt("keep", 0);
    let mut c = cx.conn();
    if setup != 0 {
        ddl_setup(cx, c.as_mut(), tables);
    }
    ddl_queries(cx, c.as_mut(), tables, reps, &only);
    if keep == 0 {
        ddl_drop(cx, c.as_mut(), tables);
    }
}

fn ddl_each(cx: &Ctx, c: &mut dyn Conn, what: &str, n: usize, f: &mut dyn FnMut(&mut dyn Conn, usize) -> Res<()>) {
    let t = Instant::now();
    for i in 0..n {
        must(f(c, i), what);
    }
    let s = t.elapsed().as_secs_f64();
    cx.report(
        "ddl",
        what,
        &[
            ("n", n.to_string()),
            ("total_s", format!("{s:.3}")),
            ("per_op_ms", format!("{:.3}", s * 1000.0 / n as f64)),
        ],
    );
}

fn ddl_setup(cx: &Ctx, c: &mut dyn Conn, tables: usize) {
    run_script(c, "DROP SCHEMA IF EXISTS ddl CASCADE; CREATE SCHEMA ddl");
    let each = |c: &mut dyn Conn, what: &str, n: usize, f: &mut dyn FnMut(&mut dyn Conn, usize) -> Res<()>| {
        ddl_each(cx, c, what, n, f)
    };
    each(c, "create_table", tables, &mut |c, i| {
        c.simple(&format!(
            "CREATE TABLE ddl.t{i} (id serial PRIMARY KEY, a int NOT NULL, b text, c timestamptz DEFAULT now(), d numeric(10,2) CHECK (d >= 0))"
        ))
        .map(drop)
    });
    each(c, "create_index", tables, &mut |c, i| {
        c.simple(&format!("CREATE INDEX ON ddl.t{i} (a, b)")).map(drop)
    });
    each(c, "alter_add_column", tables, &mut |c, i| {
        c.simple(&format!("ALTER TABLE ddl.t{i} ADD COLUMN e int")).map(drop)
    });
}

fn ddl_queries(cx: &Ctx, c: &mut dyn Conn, tables: usize, reps: usize, only: &str) {
    let mid = tables / 2;
    let queries: Vec<(&str, String)> = vec![
        ("information_schema_columns", format!("SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns WHERE table_schema = 'ddl' AND table_name = 't{mid}' ORDER BY ordinal_position")),
        ("information_schema_tables", "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'ddl'".into()),
        ("psql_d_lookup", format!("SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relname OPERATOR(pg_catalog.~) '^(t{mid})$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(ddl)$' COLLATE pg_catalog.default ORDER BY 2, 3")),
        ("psql_d_columns", format!("SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'ddl.t{mid}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum")),
        ("psql_d_indexes", format!("SELECT c2.relname, i.indisprimary, i.indisunique, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true) FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i WHERE c.oid = 'ddl.t{mid}'::regclass AND c.oid = i.indrelid AND i.indexrelid = c2.oid ORDER BY i.indisprimary DESC, c2.relname")),
        ("pg_class_join_attribute", "SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid WHERE c.relnamespace = 'ddl'::regnamespace".into()),
        ("orm_constraints", "SELECT con.conname, con.contype, cl.relname, pg_get_constraintdef(con.oid) FROM pg_constraint con JOIN pg_class cl ON cl.oid = con.conrelid JOIN pg_namespace n ON n.oid = cl.relnamespace WHERE n.nspname = 'ddl' ORDER BY cl.relname, con.conname".into()),
        ("regclass_cast", format!("SELECT 'ddl.t{mid}'::regclass::oid > 0")),
        // (After DDL: the catalog's rows generated again for the new catalog.)
        ("psql_d_columns_after_ddl", format!("COMMENT ON TABLE ddl.t{mid} IS 'c'; SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'ddl.t{mid}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum")),
        ("psql_d_lookup_after_ddl", format!("COMMENT ON TABLE ddl.t{mid} IS 'c'; SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relname OPERATOR(pg_catalog.~) '^(t{mid})$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(ddl)$' COLLATE pg_catalog.default ORDER BY 2, 3")),
    ];
    for (name, sql) in &queries {
        if !only.is_empty() && !only.split(',').any(|o| o == *name) {
            continue;
        }
        let (_, total) = timed(|| {
            for _ in 0..reps {
                must(c.simple(sql), name);
            }
        });
        cx.report(
            "ddl",
            name,
            &[
                ("tables", tables.to_string()),
                ("per_query_ms", format!("{:.3}", total * 1000.0 / reps as f64)),
            ],
        );
    }
}

fn ddl_drop(cx: &Ctx, c: &mut dyn Conn, tables: usize) {
    ddl_each(cx, c, "drop_table", tables, &mut |c, i| {
        c.simple(&format!("DROP TABLE ddl.t{i}")).map(drop)
    });
    run_script(c, "DROP SCHEMA ddl CASCADE");
}

// ---------------------------------------------------------------------------------------------
// Many tables: DDL and catalog queries as the catalog grows

fn scale(cx: &Ctx) {
    let tables: usize = cx.opt("tables", 10_000);
    let schemas: usize = cx.opt("schemas", 10).max(1);
    let parts: usize = cx.opt("parts", 1_000);
    let step: usize = cx.opt("step", 1_000).max(1);
    let reps: usize = cx.opt("reps", 10);
    // (keep=1: leave the objects, for a later in-process `startup` run.)
    let keep: usize = cx.opt("keep", 0);
    let mut c = cx.conn();
    for s in 0..schemas {
        run_script(c.as_mut(), &format!("DROP SCHEMA IF EXISTS sc{s} CASCADE; CREATE SCHEMA sc{s}"));
    }
    run_script(c.as_mut(), "DROP SCHEMA IF EXISTS scp CASCADE; CREATE SCHEMA scp");
    // Created in steps; each step's time per statement shows how DDL grows with the catalog.
    let steps = |c: &mut dyn Conn, what: &str, n: usize, f: &dyn Fn(usize) -> String| {
        let mut done = 0;
        while done < n {
            let m = step.min(n - done);
            let t = Instant::now();
            for i in done..done + m {
                must(c.simple(&f(i)), what);
            }
            let s = t.elapsed().as_secs_f64();
            done += m;
            cx.report(
                "scale",
                what,
                &[
                    ("objects", done.to_string()),
                    ("per_op_ms", format!("{:.3}", s * 1000.0 / m as f64)),
                ],
            );
        }
    };
    steps(c.as_mut(), "create_table", tables, &|i| {
        format!(
            "CREATE TABLE sc{}.t{i} (id int PRIMARY KEY, a text NOT NULL, b int DEFAULT 0)",
            i % schemas
        )
    });
    must(
        c.simple("CREATE TABLE scp.p (id int, k int NOT NULL) PARTITION BY RANGE (k)"),
        "partitioned",
    );
    steps(c.as_mut(), "create_partition", parts, &|j| {
        format!(
            "CREATE TABLE scp.p{j} PARTITION OF scp.p FOR VALUES FROM ({}) TO ({})",
            j * 10,
            j * 10 + 10
        )
    });
    let mid = tables / 2;
    let ms = mid % schemas;
    let few = 200.min(tables);
    let each = |c: &mut dyn Conn, what: &str, f: &dyn Fn(usize) -> String| {
        let t = Instant::now();
        for i in 0..few {
            must(c.simple(&f(i)), what);
        }
        let s = t.elapsed().as_secs_f64();
        cx.report(
            "scale",
            what,
            &[
                ("objects", tables.to_string()),
                ("per_op_ms", format!("{:.3}", s * 1000.0 / few as f64)),
            ],
        );
    };
    each(c.as_mut(), "alter_add_column", &|i| {
        format!("ALTER TABLE sc{}.t{i} ADD COLUMN c int", i % schemas)
    });
    each(c.as_mut(), "create_index", &|i| {
        format!("CREATE INDEX ON sc{}.t{i} (b)", i % schemas)
    });
    let queries: Vec<(&str, String)> = vec![
        ("psql_d_lookup", format!("SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relname OPERATOR(pg_catalog.~) '^(t{mid})$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 2, 3")),
        ("psql_d_columns", format!("SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'sc{ms}.t{mid}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum")),
        ("psql_dt_schema", format!("SELECT n.nspname, c.relname, CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table' END, pg_catalog.pg_get_userbyid(c.relowner) FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p','') AND n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 1, 2")),
        ("information_schema_columns", format!("SELECT column_name, data_type FROM information_schema.columns WHERE table_schema = 'sc{ms}' AND table_name = 't{mid}' ORDER BY ordinal_position")),
        ("pg_class_count", "SELECT count(*) FROM pg_class".into()),
        ("regclass_cast", format!("SELECT 'sc{ms}.t{mid}'::regclass::oid > 0")),
        ("select_from_table", format!("SELECT * FROM sc{ms}.t{mid} WHERE id = 1")),
    ];
    for (name, sql) in &queries {
        let (_, total) = timed(|| {
            for _ in 0..reps {
                must(c.simple(sql), name);
            }
        });
        cx.report(
            "scale",
            name,
            &[
                ("objects", tables.to_string()),
                ("per_query_ms", format!("{:.3}", total * 1000.0 / reps as f64)),
            ],
        );
    }
    if keep == 0 {
        each(c.as_mut(), "drop_table", &|i| {
            format!("DROP TABLE sc{}.t{i}", i % schemas)
        });
        let t = Instant::now();
        for s in 0..schemas {
            must(c.simple(&format!("DROP SCHEMA sc{s} CASCADE")), "drop schema");
        }
        must(c.simple("DROP SCHEMA scp CASCADE"), "drop schema");
        cx.report(
            "scale",
            "drop_schemas",
            &[("objects", tables.to_string()), ("total_s", format!("{:.3}", t.elapsed().as_secs_f64()))],
        );
    }
}

/// In-process, after `Engine::open` (reported at startup): the first connection to the
/// database and its first catalog query, then a second connection's.
fn startup_catalog(cx: &Ctx) {
    for what in ["first", "second"] {
        let (mut c, s) = timed(|| cx.conn());
        let (_, q) = timed(|| must(c.simple("SELECT count(*) FROM pg_class"), "pg_class"));
        let (_, q2) = timed(|| must(c.simple("SELECT 1"), "select 1"));
        cx.report(
            "startup",
            what,
            &[
                ("connect_s", format!("{s:.4}")),
                ("pg_class_count_s", format!("{q:.4}")),
                ("select1_s", format!("{q2:.5}")),
            ],
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Idle sessions, databases, connection setup, startup

fn idle(cx: &Ctx) {
    let sessions: usize = cx.opt("sessions", 100);
    let dbs: usize = cx.opt("dbs", 10);
    // (settle-ms: how long sessions sit idle before memory is read; the engine server gives
    // an idle connection's free memory back after 2 s.)
    let settle_ms: u64 = cx.opt("settle-ms", 500);
    let settle = || std::thread::sleep(Duration::from_millis(settle_ms));
    // Connection setup.
    let n = 50;
    let (_, s) = timed(|| {
        for _ in 0..n {
            let mut c = cx.conn();
            must(c.simple("SELECT 1"), "select 1");
        }
    });
    cx.report("idle", "connect", &[("per_connect_ms", format!("{:.3}", s * 1000.0 / n as f64))]);
    settle();
    let base = cx.mem.bytes();
    let mut conns = Vec::new();
    for _ in 0..sessions {
        let mut c = cx.conn();
        must(c.simple("SELECT count(*) FROM pg_class"), "warm");
        conns.push(c);
    }
    settle();
    let with = cx.mem.bytes();
    cx.report(
        "idle",
        "per_session",
        &[
            ("sessions", sessions.to_string()),
            ("base_mb", mb(base)),
            ("kb_per_session", format!("{:.0}", (with as f64 - base as f64) / sessions as f64 / 1024.0)),
        ],
    );
    drop(conns);
    let mut admin = cx.conn();
    for i in 0..dbs {
        let _ = admin.simple(&format!("DROP DATABASE IF EXISTS bench_idle_{i}"));
        must(admin.simple(&format!("CREATE DATABASE bench_idle_{i}")), "create database");
    }
    settle();
    let base = cx.mem.bytes();
    let mut held = Vec::new();
    for i in 0..dbs {
        let mut c = cx.target.connect(&format!("bench_idle_{i}")).unwrap();
        must(c.simple("CREATE TABLE t (a int); INSERT INTO t VALUES (1); SELECT * FROM t"), "use db");
        held.push(c);
    }
    settle();
    let with = cx.mem.bytes();
    cx.report(
        "idle",
        "per_database",
        &[
            ("dbs", dbs.to_string()),
            ("kb_per_database_with_session", format!("{:.0}", (with as f64 - base as f64) / dbs as f64 / 1024.0)),
        ],
    );
    drop(held);
    settle();
    for i in 0..dbs {
        let _ = admin.simple(&format!("DROP DATABASE bench_idle_{i}"));
    }
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let mut opts: HashMap<String, String> = HashMap::new();
    let mut workloads = Vec::new();
    while let Some(a) = args.next() {
        if let Some(k) = a.strip_prefix("--") {
            let v = args.next().unwrap_or_default();
            opts.insert(k.to_string(), v);
        } else {
            workloads.push(a);
        }
    }
    let get = |k: &str, d: &str| opts.get(k).cloned().unwrap_or_else(|| d.to_string());
    let db = get("db", "bench");
    let label = get("label", &get("target", "wire"));
    let mut mem = match (opts.get("mem-pid"), opts.get("mem-cgroup")) {
        (Some(p), _) => MemProbe::Pid(p.parse().unwrap()),
        (_, Some(c)) => MemProbe::Cgroup(c.clone()),
        _ => MemProbe::None,
    };
    let target = match get("target", "wire").as_str() {
        "inproc" => {
            let data = std::path::PathBuf::from(get("data", "/tmp/bench-engine"));
            let (engine, open_s) = timed(|| {
                Engine::open(EngineOptions {
                    data_dir: data,
                    cache_bytes: get("cache-mb", "64").parse::<usize>().unwrap() << 20,
                    sync: get("sync", "on") == "on",
                    ..EngineOptions::default()
                })
                .expect("open engine")
            });
            mem = MemProbe::Pid(std::process::id());
            println!("RESULT\t{label}\tstartup\tengine_open\tsecs={open_s:.4}");
            Target::InProc { engine }
        }
        _ => Target::Wire {
            host: get("host", "127.0.0.1"),
            port: get("port", "5433").parse().unwrap(),
            password: get("password", "pw"),
        },
    };
    let cx = Ctx {
        target,
        db,
        label,
        mem,
        opts,
    };
    for w in workloads {
        match w.as_str() {
            "createdb" => {
                let mut c = cx.target.connect("postgres").unwrap();
                let _ = c.simple(&format!("DROP DATABASE IF EXISTS {}", cx.db));
                must(c.simple(&format!("CREATE DATABASE {}", cx.db)), "create database");
            }
            "oltp-load" => oltp_load(&cx),
            "oltp" => oltp(&cx),
            "analytic-load" => analytic_load(&cx),
            "analytic" => run_queries(&cx, "analytic", ANALYTIC),
            "tpch-load" => tpch_load(&cx),
            "tpch" => run_queries(&cx, "tpch", tpch::QUERIES),
            "bulk" => bulk(&cx),
            "ddl" => ddl(&cx),
            "scale" => scale(&cx),
            "startup-catalog" => startup_catalog(&cx),
            "idle" => idle(&cx),
            "startup" => {
                let (c, s) = timed(|| cx.conn());
                let mut c = c;
                let (_, q) = timed(|| must(c.simple("SELECT 1"), "select 1"));
                cx.report("startup", "first_session", &[("connect_s", format!("{s:.4}")), ("first_query_s", format!("{q:.4}"))]);
                let table: String = cx.opt("table", String::new());
                if !table.is_empty() {
                    let (r, s) = timed(|| must(c.simple(&format!("SELECT count(*) FROM {table}")), "count"));
                    let _ = r;
                    cx.report("startup", "first_count", &[("table", table), ("secs", format!("{s:.4}"))]);
                }
            }
            other => panic!("unknown workload {other}"),
        }
    }
    if let Target::InProc { engine } = &cx.target {
        engine.shutdown();
    }
}
