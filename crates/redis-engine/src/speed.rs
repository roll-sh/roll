//! Engine-only throughput (`cargo test --release -p redis-engine speed:: -- --ignored --nocapture`):
//! redis-benchmark's workloads through `Engine::call`, without the network, to see the ceiling
//! the server's threading and I/O leave room for.

use crate::client::Client;
use crate::engine::Engine;
use bytes::Bytes;

fn run(name: &str, make: impl Fn(u64) -> Vec<Bytes>) {
    run_warm(name, &make, &make)
}

/// [`run`] with the keyspace filled by `warm` first (GET over SET's keys).
fn run_warm(name: &str, warm: &dyn Fn(u64) -> Vec<Bytes>, make: &dyn Fn(u64) -> Vec<Bytes>) {
    // `ROLL_SPEED_KEYS`: a small keyspace stays in cache, leaving the per-command overhead.
    let keys: u64 = std::env::var("ROLL_SPEED_KEYS").ok().and_then(|k| k.parse().ok()).unwrap_or(100_000);
    let mut eng = Engine::standalone();
    let mut c = Client::new(1);
    let mut x = 88172645463325252u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % keys
    };
    for n in 0..keys {
        eng.call(&mut c, &warm(n));
        eng.recycle_effects();
    }
    // Shared argument buffers, as a connection's parser makes them.
    let ops: Vec<Vec<Bytes>> = (0..2_000_000)
        .map(|_| make(next()).into_iter().map(|a| bytes::BytesMut::from(&a[..]).freeze()).collect())
        .collect();
    let n = ops.len();
    let t = std::time::Instant::now();
    for a in ops {
        // As a connection runs a command: the effect takes the argument vector.
        eng.call_lending(&mut c, &a);
        let _ = eng.lend(a);
        // The server publishes after each batch.
        eng.recycle_effects();
    }
    let ns = t.elapsed().as_nanos() as f64 / n as f64;
    eprintln!("{name:6} {ns:5.0} ns/op  {:.2}M/s", 1000.0 / ns);
}

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

/// One workload for a profiler: `ROLL_SPEED_ONLY=SET` with many rounds.
#[test]
#[ignore]
fn engine_speed_one() {
    let only = std::env::var("ROLL_SPEED_ONLY").unwrap_or_else(|_| "SET".into());
    let key = |r: u64| b(&format!("key:{r:012}"));
    for _ in 0..60 {
        match only.as_str() {
            "INCR" => run("INCR", |r| vec![b("INCR"), key(r)]),
            "GET" => run("GET", |r| vec![b("GET"), key(r)]),
            "GETHIT" => run_warm("GETHIT", &|r| vec![b("SET"), key(r), b("xxx")], &|r| vec![b("GET"), key(r)]),
            _ => run("SET", |r| vec![b("SET"), key(r), b("xxx")]),
        }
    }
}

#[test]
#[ignore]
fn engine_speed() {
    let key = |r: u64| b(&format!("key:{r:012}"));
    run("SET", |r| vec![b("SET"), key(r), b("xxx")]);
    run("GET", |r| vec![b("GET"), key(r)]);
    run_warm("GETHIT", &|r| vec![b("SET"), key(r), b("xxx")], &|r| vec![b("GET"), key(r)]);
    run("INCR", |r| vec![b("INCR"), key(r)]);
    run("LPUSH", |_| vec![b("LPUSH"), b("mylist"), b("xxx")]);
    run("HSET", |r| vec![b("HSET"), b("myhash"), b(&format!("element:{r:012}")), b("xxx")]);
    run("ZADD", |r| vec![b("ZADD"), b("myzset"), b(&format!("{:012}", r * 7 % 100_000)), b(&format!("element:{r:012}"))]);
}

#[test]
#[ignore]
fn layout_sizes() {
    use crate::value::{Entry, Str, Value};
    eprintln!(
        "Reply {} CmdResult {} Outcome {} Error {}",
        std::mem::size_of::<redis_proto::Reply>(),
        std::mem::size_of::<crate::commands::CmdResult>(),
        std::mem::size_of::<crate::engine::Outcome>(),
        std::mem::size_of::<crate::commands::Error>()
    );
    eprintln!("Str {} Value {} Entry {} dict node {}", std::mem::size_of::<Str>(), std::mem::size_of::<Value>(), std::mem::size_of::<Entry>(), crate::dict::Dict::<Str, Entry>::NODE_OVERHEAD - 16);
}
