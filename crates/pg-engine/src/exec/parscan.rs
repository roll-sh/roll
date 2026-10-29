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

/// Rows the workers dropped for bounded sorts above (`Decode::topk`; for tests).
static SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many rows the process's parallel scans dropped for bounded sorts above.
pub fn topk_skipped() -> u64 {
    SKIPPED.load(std::sync::atomic::Ordering::Relaxed)
}

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
    pub stages: Vec<Arc<super::pipe::Stage>>,
    /// An index-only scan's columns, read from the entries (`IndexChunk` gives the entry key
    /// for the row): (column, type, DESC).
    pub ios: Option<Vec<(usize, crate::types::DataType, bool)>>,
    /// Values computed over each row and put after its columns (`Node::compute_in_workers`;
    /// none where a kernel leaves one to `eval`).
    pub extra: Vec<K>,
    /// The columns a row with the values computed doesn't need any more (NULL then).
    pub drop: Vec<usize>,
    /// Of those, the ones not decoded: read only as distances' column arguments, which come
    /// from the stored bytes (`K::dist_raw`; the row decoded whole where one can't).
    pub raw: Vec<usize>,
    /// `need` without the `raw` columns (None: no raw columns).
    pub need_raw: Option<Arc<[bool]>>,
    /// A bounded sort's wish over computed value `topk.col` (`Node::topk_below`): each
    /// chunk's rows its own bounded heap discards are dropped (`Top`).
    pub topk: Option<super::TopK>,
}

/// A chunk's bounded heap of a bounded sort's keys (`Decode::topk`). A row its heap
/// discards, once the chunk has had more than twice the bound of the scan's rows, the
/// sort's own heap would discard too: the chunk's rows before it are rows of the sort's input
/// before it, so the sort's heap holds keys no greater than this one's then, and the sort
/// uses its heap at all (more than twice the bound of rows). So it needn't be sent.
struct Top {
    t: super::TopK,
    /// The scan's rows (filtered) of the chunk so far.
    seen: usize,
    /// The bound's smallest keys so far, the greatest first.
    heap: std::collections::BinaryHeap<TopKey>,
}

struct TopKey(Value, super::TopK);

impl PartialEq for TopKey {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == std::cmp::Ordering::Equal
    }
}
impl Eq for TopKey {}
impl PartialOrd for TopKey {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for TopKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.1.cmp(&self.0, &o.0)
    }
}

impl Top {
    /// Whether the scan's row with key `key` can be dropped (else it is kept in the heap).
    fn drops(&mut self, key: &Value) -> bool {
        self.seen += 1;
        if self.seen > 2 * self.t.k
            && self.heap.len() == self.t.k
            && self.heap.peek().is_some_and(|top| self.t.cmp(key, &top.0) != std::cmp::Ordering::Less)
        {
            return true;
        }
        self.heap.push(TopKey(key.clone(), self.t));
        if self.heap.len() > self.t.k {
            self.heap.pop();
        }
        false
    }
}

/// What a chunk reads: (data key, stored row) in the scan's order.
pub type Chunk = Box<dyn Iterator<Item = Result<(Vec<u8>, bytes::Bytes)>> + Send>;

/// A chunk of a secondary index's entries as the rows they point at, in index order,
/// fetched from the store a batch at a time at the statement's snapshot (`Txn::store_reader`;
/// None: an index-only scan's, the entry key standing in for the row).
pub struct IndexChunk {
    pub entries: TxnScan,
    pub fetch: Option<(Arc<crate::storage::Store>, u64)>,
    buf: VecDeque<Result<(Vec<u8>, bytes::Bytes)>>,
    done: bool,
}

/// Index entries a chunk's rows are fetched for at a time.
const FETCH: usize = 256;

impl IndexChunk {
    pub fn new(entries: TxnScan, fetch: Option<(Arc<crate::storage::Store>, u64)>) -> IndexChunk {
        IndexChunk { entries, fetch, buf: VecDeque::new(), done: false }
    }
}

impl Iterator for IndexChunk {
    type Item = Result<(Vec<u8>, bytes::Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(x) = self.buf.pop_front() {
                return Some(x);
            }
            if self.done {
                return None;
            }
            let mut ents: Vec<(Vec<u8>, bytes::Bytes)> = Vec::with_capacity(FETCH);
            while ents.len() < FETCH {
                match self.entries.next() {
                    Some(Ok(e)) => ents.push(e),
                    Some(Err(e)) => {
                        self.done = true;
                        self.buf.push_back(Err(e));
                        break;
                    }
                    None => {
                        self.done = true;
                        break;
                    }
                }
            }
            let err = self.buf.pop_back();
            match &self.fetch {
                None => {
                    // (Rows an index-only scan makes from its entries, as `index_row` counts.)
                    super::scan::ROWS_FROM_INDEX_ENTRIES.fetch_add(ents.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    for (k, d) in ents {
                        self.buf.push_back(Ok((d.to_vec(), bytes::Bytes::from(k))));
                    }
                }
                Some((store, ts)) if !ents.is_empty() => {
                    let mut keys: Vec<&[u8]> = ents.iter().map(|(_, d)| &d[..]).collect();
                    keys.sort_unstable();
                    keys.dedup();
                    match store.get_versions(&keys, *ts) {
                        Err(e) => {
                            self.done = true;
                            self.buf.push_back(Err(e));
                        }
                        Ok(got) => {
                            let found: std::collections::HashMap<&[u8], bytes::Bytes> = keys
                                .iter()
                                .zip(got)
                                .filter_map(|(k, v)| Some((*k, v?.1?)))
                                .collect();
                            for (_, d) in &ents {
                                if let Some(b) = found.get(&d[..]) {
                                    self.buf.push_back(Ok((d.to_vec(), b.clone())));
                                }
                            }
                        }
                    }
                }
                Some(_) => {}
            }
            self.buf.extend(err);
        }
    }
}

impl Decode {
    pub(super) fn row(&self, key: &[u8], bytes: &[u8]) -> Result<Row> {
        self.row_with(key, bytes, self.need.as_ref())
    }

    /// The row with the columns `need` marks decoded (None: all).
    fn row_with(&self, key: &[u8], bytes: &[u8], need: Option<&Arc<[bool]>>) -> Result<Row> {
        if let Some(cols) = &self.ios {
            // (The index entry: past the table and index ids, its key columns.)
            let mut row = vec![Value::Null; self.missing.len()];
            let mut pos = 8;
            for (col, ty, desc) in cols {
                match crate::storage::keys::decode_index_value(bytes, &mut pos, ty, *desc) {
                    Some(v) => row[*col] = v,
                    None => return Err(internal("an index entry that doesn't decode")),
                }
            }
            if self.with_key {
                row.push(Value::Bytea(Arc::from(key)));
            }
            return Ok(row);
        }
        let data = super::scan::row_data(bytes)?;
        let mut row = match need {
            Some(n) => crate::storage::row::decode_row_need(data, n)?,
            None => crate::storage::row::decode_row(data)?,
        };
        let ncols = self.missing.len();
        if row.len() < ncols {
            for i in row.len()..ncols {
                let needed = need.is_none_or(|n| n.get(i).copied().unwrap_or(false));
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

    /// `row` with the values of `extra` after its columns and the columns `drop` NULL
    /// (`done`: the filter is done with it, so nothing else reads them), when the kernels
    /// give them all; else the row as `row` makes it (the executor's thread evaluates them).
    /// `row` was decoded without the `raw` columns when `need_raw` is set: their distances
    /// come from the stored bytes.
    fn with_extra(&self, key: &[u8], bytes: &[u8], mut row: Row, done: bool) -> Result<Row> {
        let data = match self.need_raw {
            Some(_) => Some(super::scan::row_data(bytes)?),
            None => None,
        };
        let mut vals = Vec::with_capacity(self.extra.len());
        for k in &self.extra {
            let v = match (data, k.dist_col()) {
                (Some(data), Some((c, ..))) if self.raw.contains(&c) => k.dist_raw(data),
                _ => k.run(&row),
            };
            match v {
                Some(v) => vals.push(v),
                None if data.is_some() => return self.row(key, bytes),
                None => return Ok(row),
            }
        }
        if done {
            for &c in &self.drop {
                if let Some(x) = row.get_mut(c) {
                    *x = Value::Null;
                }
            }
        }
        row.extend(vals);
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
    Done(u32, u32),
}

struct Job {
    scan: Chunk,
    out: Sender<Msg>,
}

pub struct ParScan {
    /// Chunks not handed out yet, in key order.
    pending: VecDeque<Chunk>,
    /// The handed-out chunks' results, in key order.
    results: VecDeque<Receiver<Msg>>,
    jobs: Option<Sender<Job>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    batch: std::vec::IntoIter<Item>,
    /// Rows read at the ends of chunks taken, not counted with a row yet.
    carry: u32,
    /// Rows the workers dropped for a bounded sort above (`Decode::topk`), not taken yet.
    pub skipped: usize,
    /// Rows of an aggregating scan's segment left to the serial path.
    seg: std::vec::IntoIter<(Row, u8, bool)>,
    /// Set when the scan is dropped: the workers stop.
    stop: Arc<std::sync::atomic::AtomicBool>,
}

fn work(jobs: Arc<Mutex<Receiver<Job>>>, d: Arc<Decode>, stop: Arc<std::sync::atomic::AtomicBool>) {
    let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed);
    // (One for the worker's chunks: its lookups' cache lasts.)
    let mut scratch = super::pipe::Scratch::default();
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
            if !work_partial(scan, &out, &d, spec, &stopped, &mut scratch) {
                return;
            }
            continue;
        }
        let mut batch = Vec::with_capacity(BATCH);
        // (Rows read since the last item sent.)
        let mut read = 0u32;
        let mut n = 0u32;
        let mut err = None;
        let mut top = d.topk.map(|t| Top { t, seen: 0, heap: std::collections::BinaryHeap::new() });
        let mut skipped = 0u32;
        // (A row's length before its computed values.)
        let base = d.missing.len() + usize::from(d.with_key);
        for r in scan {
            n = n.wrapping_add(1);
            if n % 1024 == 1023 && stopped() {
                return;
            }
            read += 1;
            let (key, bytes) = match r {
                Ok(kv) => kv,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            };
            let row = match d.row_with(&key, &bytes, d.need_raw.as_ref().or(d.need.as_ref())) {
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
            let row = if d.extra.is_empty() {
                row
            } else {
                match d.with_extra(&key, &bytes, row, done) {
                    Ok(row) => row,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            };
            // (A row the filter is done with is the sort's: counted, and with its key, kept
            // in the chunk's heap or dropped.)
            if done && let Some(t) = &mut top {
                match row.get(base + t.t.col) {
                    Some(key) if t.drops(key) => {
                        skipped += 1;
                        continue;
                    }
                    Some(_) => {}
                    None => t.seen += 1,
                }
            }
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
        // (The chunk's lookups counted before its end is.)
        scratch.flush(&d.stages);
        let _ = out.send(match err {
            Some(e) => Msg::Err(e),
            None => Msg::Done(read, skipped),
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
    scan: Chunk,
    out: &Sender<Msg>,
    d: &Decode,
    spec: &super::partial::Spec,
    stopped: &dyn Fn() -> bool,
    scratch: &mut super::pipe::Scratch,
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
                super::pipe::run_stages(&d.stages, 0, row, scratch, &mut |r, lvl| {
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
    scratch.flush(&d.stages);
    send_part(&mut pa, &mut read) && out.send(Msg::Done(0, 0)).is_ok()
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
    pub fn start(scans: Vec<Chunk>, workers: usize, pool: usize, d: Decode) -> Option<ParScan> {
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
            skipped: 0,
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
                Ok(Msg::Done(read, skipped)) => {
                    self.skipped += skipped as usize;
                    SKIPPED.fetch_add(skipped as u64, std::sync::atomic::Ordering::Relaxed);
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
