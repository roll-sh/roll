//! A sampling heap profiler for the server (`--features heapprof`, `ROLL_HEAPPROF=<file>`):
//! about one allocation per `ROLL_HEAPPROF_RATE` bytes (default 256 kB) is recorded with its
//! stack, and every few seconds `<file>.live` / `<file>.peak` get the sampled live bytes by
//! stack (the peak's as of the largest live total so far), with `/proc/self/maps` in
//! `<file>.maps`. `tools/heapprof.py` resolves and sums them. Live totals are exact within
//! 256 kB per thread. Not for production: a free of a sampled-looking pointer takes a lock.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};

pub struct Prof;

static INNER: mimalloc::MiMalloc = mimalloc::MiMalloc;
static RATE: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);
/// Sampled pointers' hashes (counts), so a free looks at the table only for those.
static MARKS: [AtomicU8; 1 << 20] = [const { AtomicU8::new(0) }; 1 << 20];

struct Table {
    /// Sampled live allocations: pointer -> (stack, weight).
    live: HashMap<usize, (u32, i64)>,
    stacks: Vec<Vec<usize>>,
    ids: HashMap<Vec<usize>, u32>,
    /// Live weight per stack, now and at the peak.
    by_stack: Vec<i64>,
    at_peak: Vec<i64>,
    peak_total: i64,
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);

thread_local! {
    /// (unflushed live delta, bytes until the next sample, inside the profiler)
    static T: Cell<(i64, i64, bool)> = const { Cell::new((0, 0, false)) };
}

unsafe extern "C" {
    fn backtrace(buf: *mut *mut c_void, size: i32) -> i32;
}

fn mark(p: usize) -> &'static AtomicU8 {
    &MARKS[((p >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 44) & ((1 << 20) - 1)]
}

fn account(delta: i64) -> bool {
    T.with(|t| {
        let (mut d, mut left, busy) = t.get();
        d += delta;
        if d.abs() > 256 << 10 {
            let now = LIVE.fetch_add(d, Ordering::Relaxed) + d;
            PEAK.fetch_max(now, Ordering::Relaxed);
            d = 0;
        }
        let rate = RATE.load(Ordering::Relaxed) as i64;
        let mut sample = false;
        if delta > 0 && rate > 0 && !busy {
            left -= delta;
            if left <= 0 {
                sample = true;
                left = rate;
            }
        }
        t.set((d, left, busy));
        sample
    })
}

fn busy(b: bool) {
    T.with(|t| {
        let (d, l, _) = t.get();
        t.set((d, l, b));
    });
}

fn record(p: usize, size: usize) {
    busy(true);
    let mut buf = [std::ptr::null_mut::<c_void>(); 40];
    // SAFETY: backtrace writes at most `buf.len()` frames into `buf`.
    let n = unsafe { backtrace(buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    let st: Vec<usize> = buf[2.min(n)..n].iter().map(|f| *f as usize).collect();
    let w = (RATE.load(Ordering::Relaxed) as i64).max(size as i64);
    if let Ok(mut g) = TABLE.lock()
        && let Some(tb) = g.as_mut()
    {
        let id = match tb.ids.get(&st) {
            Some(&i) => i,
            None => {
                let i = tb.stacks.len() as u32;
                tb.stacks.push(st.clone());
                tb.ids.insert(st, i);
                tb.by_stack.push(0);
                i
            }
        };
        tb.by_stack[id as usize] += w;
        if let Some((old, ow)) = tb.live.insert(p, (id, w)) {
            tb.by_stack[old as usize] -= ow;
        } else {
            mark(p).fetch_add(1, Ordering::Relaxed);
        }
        let total = LIVE.load(Ordering::Relaxed);
        if total > tb.peak_total + (tb.peak_total >> 5) + (16 << 20) {
            tb.peak_total = total;
            tb.at_peak = tb.by_stack.clone();
        }
    }
    busy(false);
}

fn forget(p: usize) {
    let m = mark(p);
    // (Frees inside the profiler, its table's own, would take the lock it holds.)
    if m.load(Ordering::Relaxed) == 0 || T.with(|t| t.get().2) {
        return;
    }
    busy(true);
    if let Ok(mut g) = TABLE.lock()
        && let Some(tb) = g.as_mut()
        && let Some((id, w)) = tb.live.remove(&p)
    {
        tb.by_stack[id as usize] -= w;
        m.fetch_sub(1, Ordering::Relaxed);
    }
    busy(false);
}

unsafe impl GlobalAlloc for Prof {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded as is.
        let p = unsafe { INNER.alloc(l) };
        if !p.is_null() && account(l.size() as i64) {
            record(p as usize, l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded as is.
        let p = unsafe { INNER.alloc_zeroed(l) };
        if !p.is_null() && account(l.size() as i64) {
            record(p as usize, l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        forget(p as usize);
        account(-(l.size() as i64));
        // SAFETY: forwarded as is.
        unsafe { INNER.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        forget(p as usize);
        // SAFETY: forwarded as is.
        let q = unsafe { INNER.realloc(p, l, new) };
        account(-(l.size() as i64));
        if !q.is_null() && account(new as i64) {
            record(q as usize, new);
        }
        q
    }
}

/// Starts the profiler when `ROLL_HEAPPROF` names a file.
pub fn start() {
    let Ok(path) = std::env::var("ROLL_HEAPPROF") else { return };
    let rate = std::env::var("ROLL_HEAPPROF_RATE").ok().and_then(|r| r.parse().ok()).unwrap_or(256 << 10);
    busy(true);
    *TABLE.lock().unwrap() = Some(Table {
        live: HashMap::new(),
        stacks: Vec::new(),
        ids: HashMap::new(),
        by_stack: Vec::new(),
        at_peak: Vec::new(),
        peak_total: 0,
    });
    busy(false);
    RATE.store(rate, Ordering::Relaxed);
    let _ = std::fs::copy("/proc/self/maps", format!("{path}.maps"));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            busy(true);
            dump(&path);
            busy(false);
        }
    });
}

fn dump(path: &str) {
    let (live, peak) = {
        let g = TABLE.lock().unwrap();
        let tb = g.as_ref().unwrap();
        let rows = |v: &[i64]| {
            let mut s = String::new();
            for (i, w) in v.iter().enumerate().filter(|(_, w)| **w > 0) {
                let st: Vec<String> = tb.stacks[i].iter().map(|a| format!("{a:x}")).collect();
                s.push_str(&format!("{w}\t{}\n", st.join(" ")));
            }
            s
        };
        (
            format!("# live {}\n{}", LIVE.load(Ordering::Relaxed), rows(&tb.by_stack)),
            format!("# peak {} (table at {})\n{}", PEAK.load(Ordering::Relaxed), tb.peak_total, rows(&tb.at_peak)),
        )
    };
    let _ = std::fs::write(format!("{path}.live"), live);
    let _ = std::fs::write(format!("{path}.peak"), peak);
}
