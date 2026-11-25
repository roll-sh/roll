//! (Local measuring tool of agent 306, not committed.) A table of A322_ROWS rows compacted into
//! L1, a sparse L0 file of other rows' new versions (every 97th row, none in the probed
//! ranges), then for each (where, len, n): n rows of a len-row range updated (evenly spread),
//! left in an L0 file (where = l0) or in the memtable (mem); A322_REPS raw range reads of it
//! through `fill_batch` (as `fetch_dense_ref` reads them), user instructions a read counted by
//! perf_event_open on this thread. Run in a container that may open counters.
use super::*;

fn counter() -> i32 {
    // perf_event_attr: type HARDWARE, size 128, config INSTRUCTIONS, exclude_kernel + exclude_hv.
    let mut a = [0u64; 16];
    a[0] = 128u64 << 32;
    a[1] = 1;
    a[5] = (1 << 5) | (1 << 6);
    // SAFETY: a plain system call with an attribute block of its size.
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, a.as_ptr(), 0, -1, -1, 0) } as i32;
    assert!(fd >= 0, "perf_event_open");
    fd
}

fn read_counter(fd: i32) -> u64 {
    let mut v = 0u64;
    // SAFETY: reads 8 bytes into a u64.
    let n = unsafe { libc::read(fd, &mut v as *mut u64 as *mut libc::c_void, 8) };
    assert_eq!(n, 8);
    v
}

#[test]
#[ignore]
fn a322_probe() {
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let rows = env("A322_ROWS", 400_000);
    let reps = env("A322_REPS", 2000) as usize;
    let key = |i: u64| {
        let mut k = keys::index_prefix(16385, 0);
        k.push(0x05);
        k.extend_from_slice(&i.to_be_bytes());
        k
    };
    let val = |i: u64, g: u8| {
        let mut v = vec![(i % 251) as u8 ^ g; 112];
        v[0] = row::ROW_HEADER;
        v[1..9].copy_from_slice(&i.to_le_bytes());
        Bytes::from(v)
    };
    let fd = counter();
    let mut out = String::new();
    let cases: Vec<(&str, u64, u64)> = std::env::var("A322_CASES")
        .unwrap_or_else(|_| "l0:100:0,l0:100:10,l0:100:100,l0:1000:0,l0:1000:10,l0:1000:100,mem:100:10,mem:100:100,mem:1000:10,mem:1000:100,both:100:10,both:1000:10,both:1000:100".into())
        .split(',')
        .map(|c| {
            let p: Vec<&str> = c.split(':').collect();
            (match p[0] { "mem" => "mem", "both" => "both", _ => "l0" }, p[1].parse().unwrap(), p[2].parse().unwrap())
        })
        .collect();
    for (where_, len, n) in cases {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), StoreOptions { sync: false, background: false, cache: crate::storage::cache::BlockCache::new(1 << 30), ..Default::default() }).unwrap();
        for c in 0..10 {
            let w: Vec<_> = (c * rows / 10..(c + 1) * rows / 10).map(|i| (key(i), Some(val(i, 0)))).collect();
            s.commit(w, |_| Ok(())).unwrap();
        }
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        // The probed ranges: 64 of them, spread over the table; the background L0 entries
        // outside them.
        let starts: Vec<u64> = (0..64u64).map(|j| rows / 70 * (j + 1)).collect();
        let inr = |i: u64| starts.iter().any(|&a| i >= a && i < a + len);
        let w: Vec<_> = (0..rows).filter(|i| i % 97 == 3 && !inr(*i)).map(|i| (key(i), Some(val(i, 1)))).collect();
        s.commit(w, |_| Ok(())).unwrap();
        let mut upd = Vec::new();
        for &a in &starts {
            for j in 0..n {
                upd.push((key(a + j * len / n.max(1)), Some(val(a + j, 2))));
            }
        }
        if where_ == "both" {
            // (Every other one into an L0 file, the rest left in the memtable: three sources.)
            let (a, b): (Vec<_>, Vec<_>) = upd.into_iter().enumerate().partition(|(j, _)| j % 2 == 0);
            if !a.is_empty() {
                s.commit(a.into_iter().map(|x| x.1).collect(), |_| Ok(())).unwrap();
            }
            s.flush_memtables().unwrap();
            if !b.is_empty() {
                s.commit(b.into_iter().map(|x| x.1).collect(), |_| Ok(())).unwrap();
            }
        } else if where_ == "l0" {
            if !upd.is_empty() {
                s.commit(upd, |_| Ok(())).unwrap();
            }
            s.flush_memtables().unwrap();
        } else {
            s.flush_memtables().unwrap();
            if !upd.is_empty() {
                s.commit(upd, |_| Ok(())).unwrap();
            }
        }
        let ts = s.visible_ts();
        let mut b = batch::RowBatch::default();
        let mut hash = 0u64;
        let mut read = |a: u64, b: &mut batch::RowBatch, check: bool| {
            let (st, en) = (key(a), key(a + len));
            let mut sc = s.scan_raw(&st, &en, ts);
            b.clear();
            sc.fill_batch(b, len as usize).unwrap();
            if check {
                assert_eq!(b.len(), len as usize);
                for j in 0..b.len() {
                    let (k, v) = b.get(j);
                    hash = hash.wrapping_mul(31).wrapping_add(u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap())).wrapping_add(v[20] as u64);
                }
            }
        };
        for &a in &starts {
            read(a, &mut b, true);
        }
        let c0 = read_counter(fd);
        for r in 0..reps {
            read(starts[r % starts.len()], &mut b, false);
        }
        let c1 = read_counter(fd);
        let line = format!("{where_} len {len} n {n}: {} user instructions a read (hash {hash:x})\n", (c1 - c0) / reps as u64);
        eprint!("{line}");
        out.push_str(&line);
    }
    eprintln!("RESULT\n{out}");
}
