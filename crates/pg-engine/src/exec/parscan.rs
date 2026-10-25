//! Parallel sequential scans: a big table's key range is cut into chunks that worker
//! threads read, decode (only the columns needed) and filter (when the filter compiles to
//! a kernel), while the executor's thread takes the chunks' rows in key order: the rows,
//! their order and the errors are a serial scan's. A row whose filter a kernel leaves to
//! `eval` (or would fail on) comes unfiltered, and the executor's thread evaluates the
//! filter itself, as a serial scan does. The workers may also run hash join probes over
//! the rows (`pipe`) and aggregate what comes out (`partial`).

use super::Row;
use super::kernel::K;
use crate::error::{Error, Result, internal};
use crate::txn::TxnScan;
use crate::types::Value;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Parallel scans started in the process (tests check they ran).
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many parallel scans the process has started.
pub fn started() -> u64 {
    STARTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Parallel scans started with hash join probes in their workers (`pipe`; for tests).
static PIPELINED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many parallel scans the process has started with join probes.
pub fn pipelined() -> u64 {
    PIPELINED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Worker threads running in the process, against max_parallel_workers (the pool
/// Postgres's parallel workers come from): a scan gets what is left of it.
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Take up to `want` workers of a pool of `pool` (how many it got).
fn reserve(want: usize, pool: usize) -> usize {
    use std::sync::atomic::Ordering::{AcqRel, Acquire};
    let mut a = ACTIVE.load(Acquire);
    loop {
        let got = want.min(pool.saturating_sub(a));
        match ACTIVE.compare_exchange_weak(a, a + got, AcqRel, Acquire) {
            Ok(_) => return got,
            Err(now) => a = now,
        }
    }
}

/// Rows a worker sends at a time.
const BATCH: usize = 512;
/// Chunks per worker: small enough that the rows read ahead stay few.
const CHUNKS_PER_WORKER: usize = 64;
/// Chunks read ahead of the one being taken, per worker.
const AHEAD: usize = 2;

/// What every worker needs to make a stored row the scan's row.
pub struct Decode {
    /// The table columns decoded (None: all).
    pub need: Option<Arc<[bool]>>,
    /// Per column, the value of a row stored before it was added.
    pub missing: Vec<Option<Value>>,
    /// The row's storage key goes after its columns.
    pub with_key: bool,
    /// The filter as a kernel (None: none, or one the executor's thread evaluates).
    pub filter: Option<K>,
    /// The scan has a filter.
    pub filtered: bool,
    /// The rows aggregated as they're read (`partial`), rather than sent.
    pub spec: Option<Arc<super::partial::Spec>>,
    /// Hash join probes run over the filtered rows, bottom first (`pipe`).
    pub stages: Vec<Arc<super::pipe::Probe>>,
}

impl Decode {
    fn row(&self, key: &[u8], bytes: &[u8]) -> Result<Row> {
        let data = super::scan::row_data(bytes)?;
        let mut row = match &self.need {
            Some(n) => crate::storage::row::decode_row_need(data, n)?,
            None => crate::storage::row::decode_row(data)?,
        };
        let ncols = self.missing.len();
        if row.len() < ncols {
            for i in row.len()..ncols {
                let needed = self.need.as_ref().is_none_or(|n| n.get(i).copied().unwrap_or(false));
                row.push(match (&self.missing[i], needed) {
                    (Some(v), true) => v.clone(),
                    _ => Value::Null,
                });
            }
        }
        row.truncate(ncols);
        if self.with_key {
            row.push(Value::Bytea(Arc::from(key)));
        }
        Ok(row)
    }
}

/// A row, the pipeline stages it went through (`pipe`; 0: the scan's row), whether the
/// filter is done with it, and how many of the table's rows were read since the item
/// before (the filter's and the joins' dropped ones too: the table's statistics count
/// them).
type Item = (Row, u8, bool, u32);

/// What the scan gives the executor's thread: a row (`Item`'s fields) or a part.
pub enum Piece {
    Row(Row, u8, bool, u32),
    Part(super::partial::Part),
}

enum Msg {
    Rows(Vec<Item>),
    Seg(super::partial::Seg),
    Err(Error),
    /// The chunk's end, after this many dropped rows.
    Done(u32),
}

struct Job {
    scan: TxnScan,
    out: Sender<Msg>,
}

pub struct ParScan {
    /// Chunks not handed out yet, in key order.
    pending: VecDeque<TxnScan>,
    /// The handed-out chunks' results, in key order.
    results: VecDeque<Receiver<Msg>>,
    jobs: Option<Sender<Job>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    batch: std::vec::IntoIter<Item>,
    /// Rows read at the ends of chunks taken, not counted with a row yet.
    carry: u32,
    /// Rows of an aggregating scan's segment left to the serial path.
    seg: std::vec::IntoIter<(Row, u8, bool)>,
    /// Set when the scan is dropped: the workers stop.
    stop: Arc<std::sync::atomic::AtomicBool>,
}

fn work(jobs: Arc<Mutex<Receiver<Job>>>, d: Arc<Decode>, stop: Arc<std::sync::atomic::AtomicBool>) {
    let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let job = match jobs.lock() {
            Ok(rx) => rx.recv(),
            Err(_) => return,
        };
        let Ok(Job { scan, out }) = job else { return };
        if stopped() {
            return;
        }
        if let Some(spec) = &d.spec {
            if !work_partial(scan, &out, &d, spec, &stopped) {
                return;
            }
            continue;
        }
        let mut batch = Vec::with_capacity(BATCH);
        // (Rows read since the last item sent.)
        let mut read = 0u32;
        let mut n = 0u32;
        let mut err = None;
        let mut scratch = super::pipe::Scratch::default();
        for r in scan {
            n = n.wrapping_add(1);
            if n % 1024 == 1023 && stopped() {
                return;
            }
            read += 1;
            let row = r.and_then(|(k, v)| d.row(&k, &v));
            let row = match row {
                Ok(row) => row,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            };
            let done = match (&d.filter, d.filtered) {
                (_, false) => true,
                (Some(f), true) => match f.passes(&row) {
                    Some(true) => true,
                    Some(false) => continue,
                    None => false,
                },
                (None, true) => false,
            };
            if !done || d.stages.is_empty() {
                batch.push((row, 0, done, std::mem::take(&mut read)));
            } else {
                super::pipe::run_stages(&d.stages, 0, row, &mut scratch, &mut |r, lvl| {
                    batch.push((r.into_row(), lvl, true, std::mem::take(&mut read)));
                });
            }
            if batch.len() >= BATCH && out.send(Msg::Rows(std::mem::replace(&mut batch, Vec::with_capacity(BATCH)))).is_err() {
                // (The scan was dropped: nothing reads on.)
                return;
            }
        }
        if !batch.is_empty() && out.send(Msg::Rows(batch)).is_err() {
            return;
        }
        let _ = out.send(match err {
            Some(e) => Msg::Err(e),
            None => Msg::Done(read),
        });
    }
}

impl ParScan {
    /// Rows dropped after the last row returned (at the scan's end).
    pub fn trailing(&mut self) -> u32 {
        std::mem::take(&mut self.carry)
    }
}

/// Parts sent at a time, at most (rows aggregated).
const PART_ROWS: usize = 1 << 16;

/// A chunk aggregated (`partial`): false when nothing receives any more.
fn work_partial(
    scan: TxnScan,
    out: &Sender<Msg>,
    d: &Decode,
    spec: &super::partial::Spec,
    stopped: &dyn Fn() -> bool,
) -> bool {
    use super::partial::{Partials, Seg};
    let mut pa = Partials::new(spec);
    // (Rows read and not counted in a part yet.)
    let mut read = 0u64;
    let send_part = |pa: &mut Partials, read: &mut u64| {
        pa.read(std::mem::take(read));
        match pa.take() {
            Some(p) => out.send(Msg::Seg(Seg::Part(p))).is_ok(),
            None => true,
        }
    };
    let top = d.stages.len() as u8;
    let mut scratch = super::pipe::Scratch::default();
    // (Rows a part can't take: sent in their place, the part so far first.)
    let mut serial: Vec<(Row, u8, bool)> = Vec::new();
    let mut n = 0usize;
    for r in scan {
        n += 1;
        if n.is_multiple_of(4096) && stopped() {
            return false;
        }
        read += 1;
        let row = match r.and_then(|(k, v)| d.row(&k, &v)) {
            Ok(row) => row,
            Err(e) => {
                return send_part(&mut pa, &mut read) && out.send(Msg::Err(e)).is_ok();
            }
        };
        let pass = match &d.filter {
            Some(f) => f.passes(&row),
            None if d.filtered => None,
            None => Some(true),
        };
        match pass {
            Some(false) => continue,
            None => serial.push((row, 0, false)),
            Some(true) if d.stages.is_empty() => {
                if !pa.row(&row) {
                    serial.push((row, 0, true));
                }
            }
            Some(true) => {
                // (Each joined row in order: aggregated while nothing went to the serial
                // path before it, else after it, in its place.)
                super::pipe::run_stages(&d.stages, 0, row, &mut scratch, &mut |r, lvl| {
                    if !serial.is_empty() || lvl != top || !pa.row(&r) {
                        serial.push((r.into_row(), lvl, true));
                    }
                });
            }
        }
        if !serial.is_empty() {
            if !send_part(&mut pa, &mut read) || out.send(Msg::Seg(Seg::Rows(std::mem::take(&mut serial)))).is_err() {
                return false;
            }
        } else if pa.rows() >= PART_ROWS && !send_part(&mut pa, &mut read) {
            return false;
        }
    }
    send_part(&mut pa, &mut read) && out.send(Msg::Done(0)).is_ok()
}

/// Keys cutting [lo, hi] into `n` ranges of about equal width (as numbers over the bytes
/// after their common prefix), increasing, all above `lo` and at most `hi`.
pub fn split_keys(lo: &[u8], hi: &[u8], n: usize) -> Vec<Vec<u8>> {
    let cp = lo.iter().zip(hi).take_while(|(a, b)| a == b).count();
    let word = |k: &[u8]| {
        let mut w = [0u8; 8];
        for (i, b) in k.iter().skip(cp).take(8).enumerate() {
            w[i] = *b;
        }
        u64::from_be_bytes(w)
    };
    let (a, b) = (word(lo), word(hi));
    let mut out: Vec<Vec<u8>> = Vec::new();
    if b <= a {
        return out;
    }
    for j in 1..n as u128 {
        let v = a as u128 + (b - a) as u128 * j / n as u128;
        let mut k = lo[..cp].to_vec();
        k.extend_from_slice(&(v as u64).to_be_bytes());
        if k.as_slice() > lo && k.as_slice() <= hi && out.last().is_none_or(|l| *l < k) {
            out.push(k);
        }
    }
    out
}

impl ParScan {
    /// Read the chunks `scans` (consecutive key ranges, in order) with up to `workers`
    /// threads of a pool of `pool` (None: none could start).
    pub fn start(scans: Vec<TxnScan>, workers: usize, pool: usize, d: Decode) -> Option<ParScan> {
        let reserved = reserve(workers, pool);
        let workers = reserved;
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let d = Arc::new(d);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers = (0..workers)
            .filter_map(|i| {
                let (rx, d, stop) = (rx.clone(), d.clone(), stop.clone());
                std::thread::Builder::new()
                    .name(format!("roll-scan-{i}"))
                    .spawn(move || work(rx, d, stop))
                    .ok()
            })
            .collect::<Vec<_>>();
        // (Threads that didn't start go back to the pool.)
        ACTIVE.fetch_sub(reserved - workers.len(), std::sync::atomic::Ordering::AcqRel);
        if workers.is_empty() {
            return None;
        }
        STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !d.stages.is_empty() {
            PIPELINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut p = ParScan {
            pending: scans.into(),
            results: VecDeque::new(),
            jobs: Some(tx),
            batch: Vec::new().into_iter(),
            seg: Vec::new().into_iter(),
            workers: Vec::new(),
            carry: 0,
            stop,
        };
        let ahead = workers.len() * AHEAD + 1;
        p.workers = workers;
        for _ in 0..ahead {
            p.hand_out();
        }
        Some(p)
    }

    /// How many chunks to cut a scan into for `workers` threads.
    pub fn chunks(workers: usize) -> usize {
        workers * CHUNKS_PER_WORKER
    }

    fn hand_out(&mut self) {
        let Some(scan) = self.pending.pop_front() else { return };
        let (out, rx) = channel();
        self.results.push_back(rx);
        // (A send fails only once every worker is gone: `next` reports it.)
        if let Some(tx) = &self.jobs {
            let _ = tx.send(Job { scan, out });
        }
    }

    /// The next row or part (None: the scan's end).
    pub fn next_piece(&mut self) -> Result<Option<Piece>> {
        use super::partial::Seg;
        loop {
            if let Some((row, lvl, done, read)) = self.batch.next() {
                return Ok(Some(Piece::Row(row, lvl, done, read + std::mem::take(&mut self.carry))));
            }
            if let Some((row, lvl, done)) = self.seg.next() {
                return Ok(Some(Piece::Row(row, lvl, done, std::mem::take(&mut self.carry))));
            }
            let Some(rx) = self.results.front() else {
                return Ok(None);
            };
            match rx.recv() {
                Ok(Msg::Rows(rows)) => self.batch = rows.into_iter(),
                Ok(Msg::Seg(Seg::Part(p))) => return Ok(Some(Piece::Part(p))),
                Ok(Msg::Seg(Seg::Rows(rows))) => self.seg = rows.into_iter(),
                Ok(Msg::Err(e)) => {
                    self.results.clear();
                    self.pending.clear();
                    return Err(e);
                }
                Ok(Msg::Done(read)) => {
                    self.carry += read;
                    self.results.pop_front();
                    self.hand_out();
                }
                Err(_) => return Err(internal("a parallel scan worker stopped")),
            }
        }
    }
}

impl Drop for ParScan {
    fn drop(&mut self) {
        // Workers stop at their next send once nothing receives, and when the jobs end.
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.results.clear();
        self.pending.clear();
        self.jobs = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
            ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}
