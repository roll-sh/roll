//! pg-engine benchmarks: the same workloads against the engine over the wire, the engine
//! in-process (how roll embeds it) and PostgreSQL. See `tools/bench/run.sh` and BENCH.md.
//!
//!     bench --target wire --port 5433 <workload> [options]
//!     bench --target inproc --data <dir> <workload> [options]
//!
//! Workloads: oltp-load, oltp, analytic-load, analytic, tpch-load, tpch, bulk, stream-load, stream,
//! ddl, scale, idle, burst-load, burst,
//! startup, startup-catalog, vector (pgvector). Results are printed as `RESULT\t<label>\t<workload>\t<case>\t<k=v>...` lines.

mod access;
mod burst;
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
    /// A query's rows (`copy`: COPY TO's data messages) read as they come, none kept:
    /// (seconds to the first, how many).
    fn stream(&mut self, sql: &str, copy: bool) -> Res<(f64, u64)>;
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
    fn stream(&mut self, sql: &str, copy: bool) -> Res<(f64, u64)> {
        use futures::StreamExt;
        let start = Instant::now();
        let mut first = None;
        let mut n = 0u64;
        self.rt.block_on(async {
            if copy {
                let st = self.c.copy_out(sql).await.map_err(db_err)?;
                futures::pin_mut!(st);
                while let Some(b) = st.next().await {
                    b.map_err(db_err)?;
                    first.get_or_insert_with(|| start.elapsed().as_secs_f64());
                    n += 1;
                }
            } else {
                let st = self.c.simple_query_raw(sql).await.map_err(db_err)?;
                futures::pin_mut!(st);
                while let Some(m) = st.next().await {
                    if let tokio_postgres::SimpleQueryMessage::Row(_) = m.map_err(db_err)? {
                        first.get_or_insert_with(|| start.elapsed().as_secs_f64());
                        n += 1;
                    }
                }
            }
            Ok::<_, String>(())
        })?;
        Ok((first.unwrap_or(0.0), n))
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
    fn stream(&mut self, sql: &str, copy: bool) -> Res<(f64, u64)> {
        if copy {
            // (In-process COPY TO's data is collected: `Outcome::CopyOut`.)
            let (n, s) = timed(|| self.copy_out(sql));
            return Ok((s, n?));
        }
        let start = Instant::now();
        let got = std::rc::Rc::new(std::cell::Cell::new((0.0f64, 0u64)));
        let g = got.clone();
        self.s
            .query_each(sql, move |_, row| {
                if row.is_some() {
                    let (f, n) = g.get();
                    g.set((if n == 0 { start.elapsed().as_secs_f64() } else { f }, n + 1));
                }
                Ok(())
            })
            .map_err(pg_err)?;
        Ok(got.get())
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

    /// The server's CPU time so far, in seconds (user + system: the process's, or the
    /// cgroup's `usage_usec`).
    fn cpu_secs(&self) -> f64 {
        match self {
            MemProbe::None => 0.0,
            MemProbe::Pid(p) => std::fs::read_to_string(format!("/proc/{p}/stat"))
                .ok()
                .and_then(|s| {
                    // (Fields after the command's closing parenthesis; utime and stime are
                    // the 14th and 15th of the whole line.)
                    let f: Vec<&str> = s.rsplit_once(')')?.1.split_whitespace().collect();
                    Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
                })
                .unwrap_or(0.0),
            MemProbe::Cgroup(dir) => std::fs::read_to_string(format!("{dir}/cpu.stat"))
                .ok()
                .and_then(|s| {
                    let l = s.lines().find(|l| l.starts_with("usage_usec "))?;
                    Some(l.split_whitespace().nth(1)?.parse::<f64>().ok()? / 1e6)
                })
                .unwrap_or(0.0),
        }
    }

    /// `cpu_secs` in microseconds.
    fn cpu_us(&self) -> u64 {
        (self.cpu_secs() * 1e6) as u64
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
        let cpu0 = cx.mem.cpu_secs();
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
        let cpu = (cx.mem.cpu_secs() - cpu0) / times.len().max(1) as f64;
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut kv = vec![
            ("min_s", times.first().map(|t| format!("{t:.4}")).unwrap_or_default()),
            ("median_s", times.get(times.len() / 2).map(|t| format!("{t:.4}")).unwrap_or_default()),
            ("max_s", times.last().map(|t| format!("{t:.4}")).unwrap_or_default()),
            ("rows", out.0.to_string()),
            ("hash", format!("{:016x}", out.1)),
            ("peak_mem_mb", mb(peak)),
            ("cpu_s", format!("{cpu:.2}")),
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

// ---------------------------------------------------------------------------------------------
// Streaming results: a big SELECT / COPY TO read as it comes

fn stream_load(cx: &Ctx) {
    let rows: u64 = cx.opt("rows", 10_000_000);
    let mut c = cx.conn();
    let _ = c.simple("DROP TABLE IF EXISTS stream_t");
    must(c.simple("CREATE TABLE stream_t (id int, v int, t text)"), "create stream_t");
    let step = 1_000_000;
    let mut at = 0;
    while at < rows {
        let hi = (at + step).min(rows);
        let sql = format!(
            "INSERT INTO stream_t SELECT g, g % 1000, md5(g::text) FROM generate_series({}, {hi}) g",
            at + 1
        );
        must(c.simple(&sql), "load stream_t");
        at = hi;
    }
    must(c.simple("VACUUM ANALYZE stream_t"), "analyze stream_t");
    cx.report("stream-load", "rows", &[("rows", rows.to_string())]);
}

/// Time to the first row, the whole time and the server's peak memory above what it had
/// before, for the whole table as a SELECT and as COPY TO.
fn stream(cx: &Ctx) {
    let reps: usize = cx.opt("reps", 3);
    let mut c = cx.conn();
    for (case, sql, copy) in [
        ("select_all", "SELECT * FROM stream_t", false),
        ("copy_out", "COPY stream_t TO STDOUT", true),
    ] {
        for rep in 0..reps {
            let before = cx.mem.bytes();
            let m = MemSampler::start(&cx.mem);
            let (r, secs) = timed(|| must(c.stream(sql, copy), case));
            let peak = m.stop();
            cx.report(
                "stream",
                case,
                &[
                    ("rep", rep.to_string()),
                    ("rows", r.1.to_string()),
                    ("first_row_s", format!("{:.4}", r.0)),
                    ("secs", format!("{secs:.3}")),
                    ("peak_mb", mb(peak.saturating_sub(before))),
                ],
            );
        }
    }
}

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

/// DDL and catalog queries over a catalog `scale --keep 1` left (schemas `sc0`.. with tables
/// `t0`..): `n` new tables are made, altered, indexed and dropped again, so runs repeat over
/// the same catalog (for A/B runs of two builds on copies of one data directory).
fn scale_ops(cx: &Ctx) {
    let tables: usize = cx.opt("tables", 10_000);
    let schemas: usize = cx.opt("schemas", 20).max(1);
    let n: usize = cx.opt("n", 100);
    let reps: usize = cx.opt("reps", 10);
    let only: String = cx.opt("only", String::new());
    let want = |w: &str| only.is_empty() || only.split(',').any(|o| o == w);
    let mut c = cx.conn();
    let mid = tables / 2;
    let ms = mid % schemas;
    let each = |c: &mut dyn Conn, what: &str, f: &dyn Fn(usize) -> String| {
        let t = Instant::now();
        for i in 0..n {
            must(c.simple(&f(i)), what);
        }
        let s = t.elapsed().as_secs_f64();
        cx.report(
            "scale_ops",
            what,
            &[("objects", tables.to_string()), ("per_op_ms", format!("{:.3}", s * 1000.0 / n as f64))],
        );
    };
    let ddl = want("ddl");
    if ddl {
        each(c.as_mut(), "create_table", &|i| {
            format!(
                "CREATE TABLE sc{}.n{i} (id int PRIMARY KEY, a text NOT NULL, b int DEFAULT 0)",
                i % schemas
            )
        });
        each(c.as_mut(), "alter_add_column", &|i| {
            format!("ALTER TABLE sc{}.n{i} ADD COLUMN c int", i % schemas)
        });
        each(c.as_mut(), "create_index", &|i| format!("CREATE INDEX ON sc{}.n{i} (b)", i % schemas));
        each(c.as_mut(), "comment_on_table", &|i| {
            format!("COMMENT ON TABLE sc{}.n{i} IS 'x'", i % schemas)
        });
    }
    let d_columns = format!("SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'sc{ms}.t{mid}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum");
    let d_lookup = format!("SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relname OPERATOR(pg_catalog.~) '^(t{mid})$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 2, 3");
    let d_indexes = format!("SELECT c2.relname, i.indisprimary, i.indisunique, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true) FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i WHERE c.oid = 'sc{ms}.t{mid}'::regclass AND c.oid = i.indrelid AND i.indexrelid = c2.oid ORDER BY i.indisprimary DESC, c2.relname");
    let is_columns = format!("SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns WHERE table_schema = 'sc{ms}' AND table_name = 't{mid}' ORDER BY ordinal_position");
    let queries: Vec<(&str, String)> = vec![
        ("psql_d_lookup", d_lookup.clone()),
        ("psql_d_columns", d_columns.clone()),
        ("psql_d_indexes", d_indexes.clone()),
        ("information_schema_columns", is_columns.clone()),
        ("information_schema_tables", format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = 'sc{ms}'")),
        ("pg_class_count", "SELECT count(*) FROM pg_class".into()),
        ("pg_class_join_attribute", format!("SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid WHERE c.relnamespace = 'sc{ms}'::regnamespace")),
        ("orm_constraints", format!("SELECT con.conname, con.contype, cl.relname, pg_get_constraintdef(con.oid) FROM pg_constraint con JOIN pg_class cl ON cl.oid = con.conrelid JOIN pg_namespace n ON n.oid = cl.relnamespace WHERE n.nspname = 'sc{ms}' ORDER BY cl.relname, con.conname")),
        // (After DDL: a catalog version no statement has read yet.)
        ("psql_d_columns_after_ddl", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'; {d_columns}")),
        ("psql_d_lookup_after_ddl", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'; {d_lookup}")),
        ("psql_d_indexes_after_ddl", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'; {d_indexes}")),
        ("information_schema_columns_after_ddl", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'; {is_columns}")),
        // (Whole catalogs right after DDL, round 19 agent 62.)
        ("pg_class_count_after_ddl", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'; SELECT count(*) FROM pg_class")),
        ("pg_class_join_attribute_after_ddl", format!("ALTER TABLE sc{ms}.t{mid} ALTER COLUMN id SET STATISTICS 100; SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid WHERE c.relnamespace = 'sc{ms}'::regnamespace")),
        ("orm_constraints_after_ddl", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'; SELECT con.conname, con.contype, cl.relname, pg_get_constraintdef(con.oid) FROM pg_constraint con JOIN pg_class cl ON cl.oid = con.conrelid JOIN pg_namespace n ON n.oid = cl.relnamespace WHERE n.nspname = 'sc{ms}' ORDER BY cl.relname, con.conname")),
        ("comment_only", format!("COMMENT ON TABLE sc{ms}.t{mid} IS 'c'")),
    ];
    for (name, sql) in &queries {
        if !want(name) && !want("queries") {
            continue;
        }
        let (_, total) = timed(|| {
            for _ in 0..reps {
                must(c.simple(sql), name);
            }
        });
        cx.report(
            "scale_ops",
            name,
            &[("objects", tables.to_string()), ("per_query_ms", format!("{:.3}", total * 1000.0 / reps as f64))],
        );
    }
    if ddl {
        each(c.as_mut(), "drop_table", &|i| format!("DROP TABLE sc{}.n{i}", i % schemas));
    }
}

/// More kinds of objects over a catalog `scale --keep 1` left: `functions` SQL functions,
/// `views` views and `indexes` more indexes (on `t0`.. in turn), made in steps of `step` with
/// each step's time per statement (round 21, agent 92: catalogs of 50,000-100,000 objects).
fn scale_more(cx: &Ctx) {
    let tables: usize = cx.opt("tables", 10_000);
    let schemas: usize = cx.opt("schemas", 20).max(1);
    let functions: usize = cx.opt("functions", 10_000);
    let views: usize = cx.opt("views", 10_000);
    let indexes: usize = cx.opt("indexes", 10_000);
    let step: usize = cx.opt("step", 2_000).max(1);
    let mut c = cx.conn();
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
            cx.report("scale_more", what, &[("objects", done.to_string()), ("per_op_ms", format!("{:.3}", s * 1000.0 / m as f64))]);
        }
    };
    // (Names from `from` on: a second run over the same catalog.)
    let from: usize = cx.opt("from", 0);
    let t = |i: usize| (i % schemas, i % tables.max(1));
    steps(c.as_mut(), "create_function", functions, &|i| {
        let i = i + from;
        let (s, _) = t(i);
        format!("CREATE FUNCTION sc{s}.f{i}(x int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT x + {i}'")
    });
    steps(c.as_mut(), "create_view", views, &|i| {
        let i = i + from;
        let (s, k) = t(i);
        format!("CREATE VIEW sc{s}.v{i} AS SELECT id, a, b + {i} AS c FROM sc{}.t{k} WHERE id > 0", k % schemas)
    });
    steps(c.as_mut(), "create_index", indexes, &|i| {
        let i = i + from;
        let (_, k) = t(i);
        format!("CREATE INDEX ix_more_{i} ON sc{}.t{k} (a, b)", k % schemas)
    });
    // More partitions of `scp.p` (`scale` made `parts` of them).
    let parts: usize = cx.opt("parts", 1_000);
    let more_parts: usize = cx.opt("more-parts", 0);
    steps(c.as_mut(), "create_partition", more_parts, &|j| {
        let k = parts + j;
        format!("CREATE TABLE scp.p{k} PARTITION OF scp.p FOR VALUES FROM ({}) TO ({})", k * 10, k * 10 + 10)
    });
}

/// What client tools and ORMs read of the catalog (psql's `\dt` / `\df` / `\dv` / `\d t`,
/// pgAdmin's and DBeaver's table lists, Django's, Rails' and Prisma's introspection, the
/// ORM constraints query), over a catalog `scale` (and `scale-more`) made: each `reps`
/// times, the first run apart.
fn catalog_tools(cx: &Ctx) {
    let tables: usize = cx.opt("tables", 10_000);
    let schemas: usize = cx.opt("schemas", 20).max(1);
    let reps: usize = cx.opt("reps", 5);
    let only: String = cx.opt("only", String::new());
    let mid = tables / 2;
    let ms = mid % schemas;
    let t = format!("sc{ms}.t{mid}");
    let queries: Vec<(&str, String)> = vec![
        ("psql_dt", format!("SELECT n.nspname as \"Schema\", c.relname as \"Name\", CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as \"Type\", pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\" FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p','') AND n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 1,2")),
        ("psql_dv", format!("SELECT n.nspname, c.relname, pg_catalog.pg_get_userbyid(c.relowner) FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('v','') AND n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 1,2")),
        ("psql_df", format!("SELECT n.nspname as \"Schema\", p.proname as \"Name\", pg_catalog.pg_get_function_result(p.oid) as \"Result data type\", pg_catalog.pg_get_function_arguments(p.oid) as \"Argument data types\", CASE p.prokind WHEN 'a' THEN 'agg' WHEN 'w' THEN 'window' WHEN 'p' THEN 'proc' ELSE 'func' END as \"Type\" FROM pg_catalog.pg_proc p LEFT JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 1, 2, 4")),
        ("psql_d_lookup", format!("SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relname OPERATOR(pg_catalog.~) '^(t{mid})$' COLLATE pg_catalog.default AND n.nspname OPERATOR(pg_catalog.~) '^(sc{ms})$' COLLATE pg_catalog.default ORDER BY 2, 3")),
        ("psql_d_indexes", format!("SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true), pg_catalog.pg_get_constraintdef(con.oid, true), contype, condeferrable, condeferred, i.indisreplident, c2.reltablespace FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x')) WHERE c.oid = '{t}'::regclass AND c.oid = i.indrelid AND i.indexrelid = c2.oid ORDER BY i.indisprimary DESC, c2.relname")),
        ("pgadmin_tables", format!("SELECT rel.oid, rel.relname AS name, (SELECT count(*) FROM pg_catalog.pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE) AS triggercount, (SELECT count(*) FROM pg_catalog.pg_trigger WHERE tgrelid=rel.oid AND tgisinternal = FALSE AND tgenabled = 'O') AS has_enable_triggers, (CASE WHEN rel.relkind = 'p' THEN true ELSE false END) AS is_partitioned, (SELECT count(1) FROM pg_catalog.pg_inherits WHERE inhrelid=rel.oid LIMIT 1) as is_inherits, (SELECT count(1) FROM pg_catalog.pg_inherits WHERE inhparent=rel.oid LIMIT 1) as is_inherited, des.description FROM pg_catalog.pg_class rel LEFT OUTER JOIN pg_catalog.pg_description des ON (des.objoid=rel.oid AND des.objsubid=0 AND des.classoid='pg_class'::regclass) WHERE rel.relkind IN ('r','s','t','p') AND rel.relnamespace = 'sc{ms}'::regnamespace AND NOT rel.relispartition ORDER BY rel.relname")),
        ("dbeaver_tables", format!("SELECT c.oid,c.*,d.description FROM pg_catalog.pg_class c LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=c.oid AND d.objsubid=0 AND d.classoid='pg_class'::regclass WHERE c.relnamespace='sc{ms}'::regnamespace AND c.relkind not in ('i','I','c')")),
        ("django_tables", "SELECT c.relname, CASE WHEN c.relispartition THEN 'p' WHEN c.relkind IN ('m', 'v') THEN 'v' ELSE 't' END, obj_description(c.oid, 'pg_class') FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('f', 'm', 'p', 'r', 'v') AND n.nspname NOT IN ('pg_catalog', 'pg_toast') AND pg_catalog.pg_table_is_visible(c.oid)".into()),
        ("django_constraints", format!("SELECT c.conname, array(SELECT attname FROM unnest(c.conkey) WITH ORDINALITY cols(colid, arridx) JOIN pg_attribute AS ca ON cols.colid = ca.attnum WHERE ca.attrelid = c.conrelid ORDER BY cols.arridx), c.contype, (SELECT fkc.relname || '.' || fka.attname FROM pg_attribute AS fka JOIN pg_class AS fkc ON fka.attrelid = fkc.oid WHERE fka.attrelid = c.confrelid AND fka.attnum = c.confkey[1]), cl.reloptions FROM pg_constraint AS c JOIN pg_class AS cl ON c.conrelid = cl.oid WHERE cl.relname = 't{mid}' AND pg_catalog.pg_table_is_visible(cl.oid)")),
        ("rails_columns", format!("SELECT a.attname, format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod, c.collname, col_description(a.attrelid, a.attnum) AS comment, attidentity AS identity, attgenerated as attgenerated FROM pg_attribute a LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum LEFT JOIN pg_type t ON a.atttypid = t.oid LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <> t.typcollation WHERE a.attrelid = '{t}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum")),
        ("prisma_columns", format!("SELECT oid.namespace, info.table_name, info.column_name, format_type(att.atttypid, att.atttypmod) as formatted_type, info.numeric_precision, info.numeric_scale, info.numeric_precision_radix, info.datetime_precision, info.data_type, info.udt_schema as type_schema_name, info.udt_name as full_data_type, pg_get_expr(attdef.adbin, attdef.adrelid) AS column_default, info.is_nullable, info.is_identity, info.character_maximum_length, description.description FROM information_schema.columns info JOIN pg_attribute att ON att.attname = info.column_name JOIN (SELECT pg_class.oid, relname, pg_namespace.nspname as namespace FROM pg_class JOIN pg_namespace on pg_namespace.oid = pg_class.relnamespace AND pg_namespace.nspname = ANY ('{{sc{ms}}}')) as oid ON oid.oid = att.attrelid AND relname = info.table_name AND namespace = info.table_schema LEFT OUTER JOIN pg_attrdef attdef ON attdef.adrelid = att.attrelid AND attdef.adnum = att.attnum AND table_schema = namespace LEFT OUTER JOIN pg_description description ON description.objoid = att.attrelid AND description.objsubid = att.attnum WHERE table_schema = ANY ('{{sc{ms}}}') ORDER BY namespace, table_name, ordinal_position")),
        ("orm_constraints", format!("SELECT con.conname, con.contype, cl.relname, pg_get_constraintdef(con.oid) FROM pg_constraint con JOIN pg_class cl ON cl.oid = con.conrelid JOIN pg_namespace n ON n.oid = cl.relnamespace WHERE n.nspname = 'sc{ms}' ORDER BY cl.relname, con.conname")),
        ("is_tables", format!("SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = 'sc{ms}' ORDER BY table_name")),
        ("is_columns", format!("SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns WHERE table_schema = 'sc{ms}' AND table_name = 't{mid}' ORDER BY ordinal_position")),
        ("pg_class_count", "SELECT count(*) FROM pg_class".into()),
    ];
    let mut c = cx.conn();
    for (name, sql) in &queries {
        if !only.is_empty() && !only.split(',').any(|o| o == *name) {
            continue;
        }
        let (rows, first) = timed(|| must(c.simple(sql), name).0);
        let (_, total) = timed(|| {
            for _ in 0..reps {
                must(c.simple(sql), name);
            }
        });
        cx.report(
            "catalog_tools",
            name,
            &[
                ("rows", rows.to_string()),
                ("first_ms", format!("{:.3}", first * 1000.0)),
                ("per_query_ms", format!("{:.3}", total * 1000.0 / reps.max(1) as f64)),
            ],
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
            "stream-load" => stream_load(&cx),
            "stream" => stream(&cx),
            "vector" => vector_bench(&cx),
            "access-load" => access::load(&cx),
            "access" => access::run(&cx),
            "burst-load" => burst::load(&cx),
            "burst" => burst::run(&cx),
            "ddl" => ddl(&cx),
            "scale" => scale(&cx),
            "scale-ops" => scale_ops(&cx),
            "scale-more" => scale_more(&cx),
            "catalog-tools" => catalog_tools(&cx),
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

// ---------------------------------------------------------------------------------------------
// pgvector

/// A deterministic vector of `dim` floats in [-1, 1) (xorshift64*, seeded by `seed`), as
/// pgvector's text input.
fn vector_text(seed: u64, dim: usize) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut s = String::with_capacity(dim * 10);
    s.push('[');
    for i in 0..dim {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let v = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0;
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{:.5}", v));
    }
    s.push(']');
    s
}

/// A deterministic sparse vector of `dim` dimensions with `nnz` nonzero elements, as
/// sparsevec's text input.
fn sparsevec_text(seed: u64, dim: usize, nnz: usize) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let mut idx: Vec<usize> = (0..nnz.min(dim)).map(|_| (next() % dim as u64) as usize + 1).collect();
    idx.sort_unstable();
    idx.dedup();
    let els: Vec<String> = idx.iter().map(|i| format!("{i}:{:.5}", (next() >> 40) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0)).collect();
    format!("{{{}}}/{dim}", els.join(","))
}

/// A deterministic bit string of `dim` bits.
fn bit_text(seed: u64, dim: usize) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..dim)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            if x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 63 == 1 { '1' } else { '0' }
        })
        .collect()
}

/// pgvector over a table of `rows` values of type `type` (vector, halfvec, sparsevec, bit)
/// and `dim` dimensions, with a column `cat` (id % `cats`) for filtered queries. `parts`
/// (comma-separated; default all) picks what runs:
/// - load: (re)create and fill the table;
/// - scan: exact k-NN (seq scan + top-N sort) and the distance function summed over every
///   row, `reps` queries each (the median and the fastest reported);
/// - hnsw: build, `queries` k-NN queries, 10% more rows inserted, the queries again, filtered
///   queries with iterative scans (strict and relaxed order); the index dropped and the extra
///   rows deleted at the end;
/// - ivfflat: build, the queries, filtered queries with an iterative scan, the index dropped.
///
/// Each query case reports a hash of its results: with seeded builds on both sides (the
/// engine's ROLL_VECTOR_BUILD_SEED=42, pgvector built with -DIVFFLAT_BENCH -DHNSW_MEMORY)
/// equal hashes mean the same rows in the same order. Every case reports the server's peak
/// memory while it ran (`peak_mb`, above what it had before).
fn vector_bench(cx: &Ctx) {
    let rows: u64 = cx.opt("rows", 100_000);
    let dim: usize = cx.opt("dim", 128);
    let ty: String = cx.opt("type", "vector".to_string());
    let nnz: usize = cx.opt("nnz", 32);
    let cats: u64 = cx.opt("cats", 50);
    let queries: u64 = cx.opt("queries", 200);
    let reps: usize = cx.opt("reps", 5);
    let k: usize = cx.opt("k", 10);
    let lists: usize = cx.opt("lists", 100);
    let probes: usize = cx.opt("probes", 10);
    let m: usize = cx.opt("m", 16);
    let efc: usize = cx.opt("ef_construction", 64);
    let efs: usize = cx.opt("ef_search", 40);
    let parts: String = cx.opt("parts", "load,hnsw,ivfflat,scan".to_string());
    let part = |p: &str| parts.split(',').any(|x| x == p);
    let text = |seed: u64| match ty.as_str() {
        "sparsevec" => sparsevec_text(seed, dim, nnz),
        "bit" => bit_text(seed, dim),
        _ => vector_text(seed, dim),
    };
    // The operator and opclass of each type's main distance; sparsevec has no ivfflat.
    let (op, ops) = match ty.as_str() {
        "bit" => ("<~>", "bit_hamming_ops"),
        t => ("<->", if t == "halfvec" { "halfvec_l2_ops" } else if t == "sparsevec" { "sparsevec_l2_ops" } else { "vector_l2_ops" }),
    };
    let table = format!("vb_{ty}_{dim}");
    let mut c = cx.conn();
    // (hash, seconds, peak memory above the start, the server's CPU microseconds.)
    let run = |c: &mut dyn Conn, case: &str, sql: &str| -> (u64, f64, u64, u64) {
        let before = cx.mem.bytes();
        let cpu = cx.mem.cpu_us();
        let sampler = MemSampler::start(&cx.mem);
        let (r, s) = timed(|| c.simple(sql));
        let peak = sampler.stop();
        (must(r, case).1, s, peak.saturating_sub(before), cx.mem.cpu_us().saturating_sub(cpu))
    };
    run_script(c.as_mut(), "CREATE EXTENSION IF NOT EXISTS vector; SET max_parallel_maintenance_workers = 0");
    let load = |c: &mut dyn Conn, from: u64, to: u64| -> f64 {
        let mut data = String::new();
        for i in from..=to {
            data.push_str(&format!("{i}\t{}\t{}\n", i % cats, text(i)));
        }
        let (r, s) = timed(|| c.copy_in(&format!("COPY {table} FROM STDIN"), data.as_bytes()));
        must(r, "copy");
        s
    };
    if part("load") {
        run_script(c.as_mut(), &format!("DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (id int PRIMARY KEY, cat int, embedding {ty}({dim}))"));
        let s = load(c.as_mut(), 1, rows);
        run_script(c.as_mut(), &format!("VACUUM ANALYZE {table}"));
        cx.report("vector", "copy_in", &[("type", ty.clone()), ("rows", rows.to_string()), ("dim", dim.to_string()), ("secs", format!("{s:.3}"))]);
    }
    let qs: Vec<String> = (0..queries).map(|q| text(1_000_000_007 + q)).collect();
    // (Every row read once first: the caches warm on both sides, so the peaks below are the
    // cases' own.)
    run_script(c.as_mut(), &format!("SELECT count(embedding) FROM {table}"));
    let lit = |q: &str| if ty == "bit" { format!("B'{q}'") } else { format!("'{q}'") };
    let run_queries = |c: &mut dyn Conn, case: &str, filter: bool| {
        let mut lat = Vec::with_capacity(qs.len());
        let mut hash = 0u64;
        let before = cx.mem.bytes();
        let cpu = cx.mem.cpu_us();
        let sampler = MemSampler::start(&cx.mem);
        let start = Instant::now();
        for (i, q) in qs.iter().enumerate() {
            let w = if filter { format!(" WHERE cat = {}", i as u64 % cats) } else { String::new() };
            let t = Instant::now();
            let (_, h) = must(c.simple(&format!("SELECT id FROM {table}{w} ORDER BY embedding {op} {} LIMIT {k}", lit(q))), case);
            lat.push((t.elapsed().as_micros() as u32).max(1));
            hash = hash.rotate_left(5) ^ h;
        }
        let secs = start.elapsed().as_secs_f64();
        let peak = sampler.stop();
        lat.sort_unstable();
        cx.report("vector", case, &[
            ("queries", qs.len().to_string()),
            ("qps", format!("{:.1}", qs.len() as f64 / secs)),
            ("p50_ms", format!("{:.3}", pct(&lat, 0.5))),
            ("p99_ms", format!("{:.3}", pct(&lat, 0.99))),
            ("peak_mb", mb(peak.saturating_sub(before))),
            ("cpu_ms_per_query", format!("{:.3}", cx.mem.cpu_us().saturating_sub(cpu) as f64 / 1000.0 / qs.len() as f64)),
            ("hash", format!("{hash:016x}")),
        ]);
    };
    let build = |c: &mut dyn Conn, case: &str, sql: &str| {
        let (_, s, peak, cpu) = run(c, case, sql);
        cx.report("vector", case, &[("secs", format!("{s:.3}")), ("peak_mb", mb(peak)), ("cpu_s", format!("{:.3}", cpu as f64 / 1e6))]);
    };
    // An index's size as pg_relation_size reports it (KB, through COPY's byte count).
    let size = |c: &mut dyn Conn, case: &str, index: &str| {
        let sql = format!("COPY (SELECT repeat('x', (pg_relation_size('{index}') / 1024)::int)) TO STDOUT");
        let kb = must(c.copy_out(&sql), case).saturating_sub(1);
        cx.report("vector", case, &[("size_mb", format!("{:.1}", kb as f64 / 1024.0))]);
    };
    let iterative = |c: &mut dyn Conn, am: &str, modes: &[&str]| {
        for mode in modes {
            run_script(c, &format!("SET {am}.iterative_scan = {mode}"));
            run_queries(c, &format!("query_{am}_filtered_{mode}"), true);
        }
        run_script(c, &format!("SET {am}.iterative_scan = off"));
    };
    if part("hnsw") {
        build(c.as_mut(), "build_hnsw", &format!("CREATE INDEX {table}_hnsw ON {table} USING hnsw (embedding {ops}) WITH (m = {m}, ef_construction = {efc})"));
        size(c.as_mut(), "size_hnsw", &format!("{table}_hnsw"));
        run_script(c.as_mut(), &format!("SET hnsw.ef_search = {efs}"));
        run_queries(c.as_mut(), "query_hnsw", false);
        iterative(c.as_mut(), "hnsw", &["strict_order", "relaxed_order"]);
        let s = load(c.as_mut(), rows + 1, rows + rows / 10);
        cx.report("vector", "insert_hnsw", &[("rows", (rows / 10).to_string()), ("secs", format!("{s:.3}"))]);
        run_queries(c.as_mut(), "query_hnsw_after_insert", false);
        run_script(c.as_mut(), &format!("DROP INDEX {table}_hnsw; DELETE FROM {table} WHERE id > {rows}; VACUUM {table}"));
    }
    if part("ivfflat") && ty != "sparsevec" {
        build(c.as_mut(), "build_ivfflat", &format!("CREATE INDEX {table}_ivf ON {table} USING ivfflat (embedding {ops}) WITH (lists = {lists})"));
        size(c.as_mut(), "size_ivfflat", &format!("{table}_ivf"));
        run_script(c.as_mut(), &format!("SET ivfflat.probes = {probes}"));
        run_queries(c.as_mut(), "query_ivfflat", false);
        iterative(c.as_mut(), "ivfflat", &["relaxed_order"]);
        run_script(c.as_mut(), &format!("DROP INDEX {table}_ivf"));
    }
    if part("scan") {
        let mut cases = vec![("exact_knn", format!("SELECT id FROM {table} ORDER BY embedding {op} %Q LIMIT {k}"))];
        if ty == "bit" {
            cases.push(("hamming_all", format!("SELECT sum(embedding <~> %Q) FROM {table}")));
            cases.push(("jaccard_all", format!("SELECT sum(embedding <%> %Q) FROM {table}")));
        } else {
            cases.push(("exact_knn_filtered", format!("SELECT id FROM {table} WHERE cat = 7 ORDER BY embedding <-> %Q LIMIT {k}")));
            cases.push(("l2_distance_all", format!("SELECT sum(embedding <-> %Q) FROM {table}")));
            cases.push(("cosine_distance_all", format!("SELECT sum(embedding <=> %Q) FROM {table}")));
            cases.push(("inner_product_all", format!("SELECT sum(embedding <#> %Q) FROM {table}")));
        }
        // `cases`: only the scan cases named (comma-separated).
        let only: String = cx.opt("cases", String::new());
        cases.retain(|(case, _)| only.is_empty() || only.split(',').any(|x| x == *case));
        for (case, sql) in cases {
            let mut secs = Vec::new();
            let (mut hash, mut peak, mut cpu) = (0u64, 0u64, 0u64);
            for rep in 0..reps {
                let (h, s, p, u) = run(c.as_mut(), case, &sql.replace("%Q", &lit(&qs[rep % qs.len()])));
                hash = hash.rotate_left(5) ^ h;
                secs.push(s);
                peak = peak.max(p);
                cpu += u;
            }
            secs.sort_by(f64::total_cmp);
            cx.report("vector", case, &[
                ("secs", format!("{:.4}", secs[secs.len() / 2])),
                ("min", format!("{:.4}", secs[0])),
                ("peak_mb", mb(peak)),
                // (Mean over the repetitions; the engine's count is in 10 ms ticks.)
                ("cpu_ms", format!("{:.1}", cpu as f64 / 1000.0 / reps as f64)),
                ("hash", format!("{hash:016x}")),
            ]);
        }
    }
}
