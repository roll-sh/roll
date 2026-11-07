//! Scratch (agent 169, not committed): point lookups inside the store, rows in L1.
//! `a169_points [rows] [lookups] [value bytes]`
use bytes::Bytes;
use pg_engine::storage::cache::BlockCache;
use pg_engine::storage::{Store, StoreOptions};
use std::time::Instant;

fn key(i: u64) -> Vec<u8> {
    let mut k = vec![0, 0, 0x40, 0x41, 0, 0, 0, 0, 0x20];
    k.extend_from_slice(&(i ^ (1 << 63)).to_be_bytes());
    k
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).map(|a| a.parse().unwrap()).collect();
    let rows = args.first().copied().unwrap_or(1_000_000);
    let n = args.get(1).copied().unwrap_or(200_000);
    let vlen = args.get(2).copied().unwrap_or(14) as usize;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), StoreOptions { sync: false, cache: BlockCache::new(2 << 30), ..Default::default() }).unwrap();
    let val = Bytes::from((0..vlen).map(|i| (i * 7 % 251) as u8).collect::<Vec<u8>>());
    for c in 0..rows / 100_000 {
        let w: Vec<_> = (c * 100_000..(c + 1) * 100_000).map(|i| (key(i + 1), Some(val.clone()))).collect();
        s.commit(w, |_| Ok(())).unwrap();
    }
    s.flush_memtables().unwrap();
    s.compact().unwrap();
    let ts = s.visible_ts();
    // Ascending (a nested loop's inner side) and scattered (an index's row fetches).
    let asc: Vec<Vec<u8>> = (1..=n).map(key).collect();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let rnd: Vec<Vec<u8>> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            key(1 + x % rows)
        })
        .collect();
    for round in 0..3 {
        for (what, keys) in [("asc", &asc), ("rnd", &rnd)] {
            let ends: Vec<Vec<u8>> = keys.iter().map(|k| [k.as_slice(), &[0xFF]].concat()).collect();
            let mut found = 0;
            let t = Instant::now();
            for k in keys.iter() {
                found += s.get(k, ts).unwrap().is_some() as u64;
            }
            let a = t.elapsed();
            let t = Instant::now();
            for (k, e) in keys.iter().zip(&ends) {
                found += s.scan_raw(k, e, ts).next().is_some() as u64;
            }
            let c = t.elapsed();
            let t = Instant::now();
            for k in keys.iter() {
                found += s.get_versions(&[k.as_slice()], ts).unwrap()[0].is_some() as u64;
            }
            let d = t.elapsed();
            let ns = |d: std::time::Duration| d.as_nanos() as f64 / n as f64;
            println!("round {round} {what}: get {:.0} ns, scan {:.0} ns, get_versions {:.0} ns ({found})", ns(a), ns(c), ns(d));
        }
    }
}
