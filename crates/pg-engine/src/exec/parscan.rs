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
use crate::storage::batch::{FillRows, RowBatch};
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

/// Workers for another parallel operation (a parallel index build), up to `want` from what
/// is left of max_parallel_workers' pool (`pool`): how many it got, given back with
/// `release_workers`.
pub(crate) fn reserve_workers(want: usize, pool: usize) -> usize {
    reserve(want, pool)
}

pub(crate) fn release_workers(n: usize) {
    ACTIVE.fetch_sub(n, std::sync::atomic::Ordering::AcqRel);
}

thread_local! {
    /// Parallel index builds this thread's statements ran.
    static BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Parallel index builds this thread's statements ran (`session::ddl::parbuild`), for tests
/// (other tests' builds, on threads of their own, don't count).
pub fn index_builds() -> u64 {
    BUILDS.with(std::cell::Cell::get)
}

pub(crate) fn count_index_build() {
    BUILDS.with(|c| c.set(c.get() + 1));
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
    /// Per column, its declared type (`storage::cols::decode_cols` decodes by it).
    pub types: Vec<crate::types::DataType>,
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
    /// The rows come as stored (`TxnScan::raw`): the out-of-line values of the columns
    /// read come from here, read by each worker for the rows it keeps (`LazyBuf::reader`).
    pub toast: Option<Arc<crate::txn::toast::Source>>,
    /// The batch kernels (`Pre`), compiled by the first chunk for all of them (None
    /// inside: none fit).
    pub vplan: std::sync::OnceLock<Option<Arc<super::vector::AggPlan>>>,
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

/// Whether the shallow heap slots (including growth capacity) fit the budget.
pub(super) fn topk_slots_fit(k: usize, budget: usize) -> bool {
    k.checked_add(1)
        .and_then(|n| n.checked_next_power_of_two())
        .and_then(|n| n.max(4).checked_mul(size_of::<TopKey>()))
        .is_some_and(|bytes| bytes <= budget)
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
        if self.seen > self.t.k.saturating_mul(2)
            && self.heap.len() == self.t.k
            && self.heap.peek().is_some_and(|top| match self.t.cmp(key, &top.0) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => !self.t.keep_ties,
                std::cmp::Ordering::Less => false,
            })
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

/// A chunk of a bulk read (`Store::bulk_range`): the blocks it reads evict nothing from the
/// block cache (`storage::bulk_read`), as PostgreSQL's ring keeps a big table's scan out of
/// shared buffers; the probes and lookups the worker runs over its rows still fill it.
pub struct Bulk<I>(pub I);

impl<I: Iterator> Iterator for Bulk<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        crate::storage::bulk_read(|| self.0.next())
    }
}

/// `chunk` as a bulk read when `bulk`.
pub fn chunk<I: Iterator<Item = Result<(Vec<u8>, bytes::Bytes)>> + Send + 'static>(it: I, bulk: bool) -> Chunk {
    if bulk { Box::new(Bulk(it)) } else { Box::new(it) }
}

/// What a parallel or inline scan's chunk reads: a table's stored rows a batch at a time by
/// reference (`FillRows`: no key or value made per row), or rows one at a time (an index's
/// entries with their rows fetched, rows in a list).
pub enum Source {
    Fill(Box<dyn FillRows>),
    Rows(Chunk),
}

impl From<Chunk> for Source {
    fn from(c: Chunk) -> Source {
        Source::Rows(c)
    }
}

/// Rows in a list (a modelled heap's rows put in heap order, `ScanNode`) a fill at a time:
/// the batch kernels take them as they take a table's.
pub struct ListFill(pub std::vec::IntoIter<(Vec<u8>, bytes::Bytes)>);

/// Reordered heap rows actually filled into batches (tests distinguish this source).
pub static LIST_FILL_ROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl FillRows for ListFill {
    fn fill(&mut self, b: &mut RowBatch, max: usize) -> Result<()> {
        let before = b.len();
        for (k, v) in self.0.by_ref().take(max) {
            b.push(&k, v);
        }
        LIST_FILL_ROWS.fetch_add((b.len() - before) as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// Rows `DisplacedFill` put in their places (tests distinguish this source).
pub static DISPLACED_ROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A chunk of keys of a modelled heap whose rows only inserts put out of key order
/// (`sizing::displaced_inserts`), in heap order: the rows `inner` reads in key order, those
/// rows (`skip`) passed over there, and each of them (`put`) given before the first row whose
/// key is at least its anchor's (the row after it in heap order; none: at the chunk's end).
pub struct DisplacedFill<F> {
    inner: F,
    /// Keys passed over, ascending once `ready`.
    pub skip: Vec<Vec<u8>>,
    /// (place in heap order, anchor key, key, value), in heap order once `ready`.
    pub put: Vec<(u64, Option<Vec<u8>>, Vec<u8>, bytes::Bytes)>,
    batch: RowBatch,
    /// The next of `skip` and of `put`.
    at: (usize, usize),
    done: bool,
}

impl<F> DisplacedFill<F> {
    pub fn new(inner: F) -> Self {
        DisplacedFill { inner, skip: Vec::new(), put: Vec::new(), batch: RowBatch::default(), at: (0, 0), done: false }
    }

    /// Its lists in order.
    pub fn ready(mut self) -> Self {
        self.skip.sort_unstable();
        self.put.sort_unstable_by_key(|p| p.0);
        self
    }
}

impl<F: FillRows> FillRows for DisplacedFill<F> {
    fn fill(&mut self, b: &mut RowBatch, max: usize) -> Result<()> {
        let target = b.len() + max;
        while b.len() < target {
            if self.batch.remaining() == 0 && !self.done {
                self.batch.clear();
                self.inner.fill(&mut self.batch, max)?;
                self.done = self.batch.is_empty();
            }
            if self.done {
                let Some((_, _, k, v)) = self.put.get(self.at.1) else { break };
                b.push(k, v.clone());
                self.at.1 += 1;
                DISPLACED_ROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            let i = self.batch.pos();
            let (k, _) = self.batch.get(i);
            if let Some((_, Some(a), dk, dv)) = self.put.get(self.at.1)
                && a.as_slice() <= k
            {
                b.push(dk, dv.clone());
                self.at.1 += 1;
                DISPLACED_ROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            // (A displaced row the snapshot doesn't see isn't met.)
            while self.skip.get(self.at.0).is_some_and(|s| s.as_slice() < k) {
                self.at.0 += 1;
            }
            if self.skip.get(self.at.0).is_some_and(|s| s.as_slice() == k) {
                self.at.0 += 1;
            } else {
                match self.batch.value_in(i) {
                    (v, Some((buf, off))) => b.push_in(k, buf, off, v.len()),
                    (v, None) => b.push_copy(k, v),
                }
            }
            self.batch.advance();
        }
        Ok(())
    }
}

/// A table's rows in [range) through `FillRows`, as a bulk read when `bulk`.
pub fn fill_source<F: FillRows + 'static>(f: F, bulk: bool) -> Source {
    Source::Fill(if bulk { Box::new(crate::storage::batch::BulkFill(f)) } else { Box::new(f) })
}

/// Rows a fill asks for at a time.
const FILL: usize = 256;

/// A scan's batch kernels (`vector`) over a fill's rows decoded as columns
/// (`storage::cols`): the filter's outcome per row, and the bottom hash join probe's lookup
/// (`pipe::Probe::int_key_col`). A row they can't decide (`odd`: a value its column's
/// vector can't hold, one out of line; or a filter outcome `Out`) goes through the row path.
struct Pre<'a> {
    plan: Arc<super::vector::AggPlan>,
    probe: Option<&'a super::pipe::Probe>,
    types: &'a [crate::types::DataType],
    missing: Vec<Value>,
    cols: crate::storage::cols::ColBatch,
    odd: Vec<bool>,
    sel: Vec<u32>,
    tri: Vec<super::vector::Tri>,
    /// Per aggregate argument (`AggPlan::args`), each row's exact number (by row; the
    /// rows the filter passes), computed a column at a time.
    nums: Vec<Vec<super::vector::Num>>,
    /// The rows the filter passes (`tri` T, not odd), and per aggregate argument their
    /// numbers as a lane (`aggregate`).
    live: Vec<u32>,
    /// `live` is the decoded run's (`fill_live`).
    live_set: bool,
    lanes: Option<super::vector::Lanes>,
    /// Rows decoded, and of them the ones the batch did (dropped or aggregated): the rest
    /// are decoded again by the row path, so a chunk where the batch does too few stops
    /// using it (`worth`).
    seen: u64,
    done: u64,
    /// `worth` said no: `run_rows` reads on without it.
    off: bool,
}

impl<'a> Pre<'a> {
    /// `n` more rows decoded, `done` of them done by the batch; false: the batch did under
    /// a quarter of the chunk's rows so far (past 2,048 of them): not worth decoding.
    fn worth(&mut self, n: usize, done: u64) -> bool {
        self.seen += n as u64;
        self.done += done;
        VECTOR_ROWS.fetch_add(done, std::sync::atomic::Ordering::Relaxed);
        self.seen < 2048 || self.done * 4 >= self.seen
    }

    /// For an aggregating scan (`spec`: its aggregates too, where no join is between), or
    /// a scan whose rows are made (None: when a filter or a probe can drop rows). None:
    /// nothing a batch kernel does.
    fn new(d: &'a Decode, spec: Option<&super::partial::Spec>) -> Option<Pre<'a>> {
        if !vectors() {
            return None;
        }
        let probe = match d.stages.first().map(|s| &**s) {
            Some(super::pipe::Stage::Hash(p)) => p.int_key_col().map(|c| (c, p)),
            _ => None,
        };
        let plan = d.vplan.get_or_init(|| Pre::compile(d, spec, probe.map(|p| p.0)).map(Arc::new)).clone()?;
        Some(Pre {
            plan,
            probe: probe.map(|p| p.1),
            types: &d.types,
            missing: d.missing.iter().map(|m| m.clone().unwrap_or(Value::Null)).collect(),
            cols: Default::default(),
            odd: Vec::new(),
            sel: Vec::new(),
            tri: Vec::new(),
            nums: Vec::new(),
            live: Vec::new(),
            live_set: false,
            lanes: None,
            seen: 0,
            done: 0,
            off: false,
        })
    }

    /// `new`'s kernels (the same for every chunk of the scan: `Decode::vplan` keeps them).
    fn compile(d: &Decode, spec: Option<&super::partial::Spec>, probe: Option<usize>) -> Option<super::vector::AggPlan> {
        if d.ios.is_some()
            || !d.raw.is_empty()
            || d.topk.is_some()
            || (spec.is_some() && !d.extra.is_empty())
            || (d.filtered && d.filter.is_none())
        {
            return None;
        }
        if !d.stages.is_empty() && probe.is_none() && (spec.is_some() || d.filter.is_none()) {
            return None;
        }
        if spec.is_none() && probe.is_none() && d.filter.is_none() {
            return None;
        }
        super::vector::AggPlan::compile(&d.types, d.filter.as_ref(), d.filtered, spec, probe)
    }

    /// Rows `rows` of `b` decoded, `odd` and `tri` (by row of the run) filled.
    fn decode(&mut self, b: &RowBatch, rows: std::ops::Range<usize>) {
        use super::vector::Tri;
        let n = rows.len();
        self.odd.clear();
        self.odd.resize(n, true);
        // (A batch that doesn't decode: all its rows to the row path, which raises the error
        // at its row.)
        if crate::storage::cols::decode_cols(b, rows, &self.plan.need, self.types, &self.missing, &mut self.cols).is_ok() {
            self.odd.fill(false);
            for r in &self.cols.ext {
                self.odd[*r as usize] = true;
            }
            for c in self.cols.cols.iter().flatten() {
                for (r, _) in &c.odd {
                    self.odd[*r as usize] = true;
                }
            }
        }
        self.sel.clear();
        self.sel.extend((0..n as u32).filter(|r| !self.odd[*r as usize]));
        self.tri.clear();
        self.tri.resize(n, Tri::T);
        if let Some(f) = &self.plan.filter {
            f.eval(&self.cols, &self.sel, &mut self.tri);
        }
        self.live_set = false;
    }

    /// `live` for the run `decode` decoded (made only where read: `aggregate`, `eval_nums`).
    fn fill_live(&mut self) {
        use super::vector::Tri;
        if !self.live_set {
            self.live.clear();
            self.live.extend(self.sel.iter().copied().filter(|r| self.tri[*r as usize] == Tri::T));
            self.live_set = true;
        }
    }

    /// The run `decode` decoded aggregated into `pa` a batch at a time (`batch_run`):
    /// when every row is one the batch decides (none odd, none the filter leaves to the
    /// row path, no probe) and every argument's numbers make a lane. False: nothing done.
    fn aggregate(&mut self, pa: &mut super::partial::Partials<'_>) -> bool {
        use super::vector::Tri;
        if self.plan.probe.is_some() || self.sel.len() != self.odd.len() || self.tri.contains(&Tri::Out) {
            return false;
        }
        self.fill_live();
        let lanes = self.lanes.get_or_insert_with(|| super::vector::Lanes::new(&self.plan.args));
        lanes.eval(&self.cols, &self.live) && pa.batch_run(&self.plan, &self.cols, &self.live, lanes)
    }

    /// The aggregates' arguments over the rows the filter passes, by row (`batch_row`'s).
    fn eval_nums(&mut self) {
        let n = self.odd.len();
        if self.plan.args.iter().any(|a| matches!(a, super::vector::AggArg::Num(_))) {
            self.fill_live();
            self.nums.resize_with(self.plan.args.len(), Vec::new);
            for (a, v) in self.plan.args.iter().zip(&mut self.nums) {
                if let super::vector::AggArg::Num(k) = a {
                    v.clear();
                    v.resize(n, None);
                    k.eval(&self.cols, &self.live, v);
                }
            }
        }
    }

    /// What decoded row `r` comes to: `DROP` when it gives nothing (the filter rejects it,
    /// or the probe finds no candidate for it), `PASSED` when the scan's whole filter holds
    /// for it (the row path needn't check it again), `ROW` when the row path decides.
    #[inline]
    fn fate(&self, r: usize) -> u8 {
        use super::vector::Tri;
        if self.odd[r] {
            return ROW;
        }
        match self.tri[r] {
            Tri::F | Tri::N => DROP,
            Tri::T if self.probe.is_some_and(|p| self.plan.misses(p, &self.cols, r)) => DROP,
            Tri::T if self.plan.filter.is_some() => PASSED,
            Tri::T | Tri::Out => ROW,
        }
    }

    /// What each row of `rows` comes to (`fate`), into `skip` (`Reader::next_row_with`).
    fn mark(&mut self, b: &RowBatch, rows: std::ops::Range<usize>, skip: &mut Vec<u8>) {
        self.decode(b, rows.clone());
        skip.clear();
        skip.extend((0..rows.len()).map(|r| self.fate(r)));
        let n = skip.iter().filter(|s| **s == DROP).count();
        self.off = !self.worth(rows.len(), n as u64);
    }
}

/// A fill's row as a `Pre` marked it: left to the row path, dropped, or passed by the
/// scan's whole filter.
const ROW: u8 = 0;
const DROP: u8 = 1;
const PASSED: u8 = 2;

/// A chunk's rows by reference, `left` more at most (an inline scan's slice of it), the
/// batch kept from slice to slice.
struct Reader<'c> {
    src: &'c mut Source,
    batch: &'c mut RowBatch,
    cur: Option<(Vec<u8>, bytes::Bytes)>,
    left: usize,
    /// The chunk has no row left.
    done: bool,
    /// What each row of `batch` from `skip_from` on comes to (`Pre::fate`).
    skip: Vec<u8>,
    skip_from: usize,
}

impl<'c> Reader<'c> {
    fn new(src: &'c mut Source, batch: &'c mut RowBatch, left: usize) -> Reader<'c> {
        Reader { src, batch, cur: None, left, done: false, skip: Vec::new(), skip_from: 0 }
    }

    /// Whether the rows come a batch at a time (`next_run`).
    fn fills(&self) -> bool {
        matches!(self.src, Source::Fill(_))
    }

    /// The next run of a fill's rows, as positions in `batch` (all of the batch's left, up
    /// to `left`), the cursor past them. None: no row left, or a source of single rows.
    fn next_run(&mut self) -> Option<Result<std::ops::Range<usize>>> {
        let Source::Fill(f) = &mut *self.src else { return None };
        if self.left == 0 || self.done {
            return None;
        }
        if self.batch.remaining() == 0 {
            self.batch.clear();
            if let Err(e) = f.fill(self.batch, FILL) {
                return Some(Err(e));
            }
            if self.batch.is_empty() {
                self.done = true;
                return None;
            }
        }
        let start = self.batch.pos();
        let n = self.batch.remaining().min(self.left);
        self.left -= n;
        for _ in 0..n {
            self.batch.advance();
        }
        Some(Ok(start..start + n))
    }

    fn next_row(&mut self) -> Option<Result<(&[u8], &[u8])>> {
        self.next_row_with(None, &mut 0, &mut false)
    }

    /// `next_row`, a fill's rows first offered a batch at a time to `pre` (`Pre`): the
    /// rows it drops are passed over, counted in `skipped`; `passed`: the row returned
    /// passed the scan's whole filter there.
    fn next_row_with(&mut self, mut pre: Option<&mut Pre<'_>>, skipped: &mut u32, passed: &mut bool) -> Option<Result<(&[u8], &[u8])>> {
        *passed = false;
        if self.left == 0 {
            return None;
        }
        match &mut *self.src {
            Source::Fill(f) => loop {
                if self.batch.remaining() == 0 {
                    self.batch.clear();
                    self.skip.clear();
                    if let Err(e) = f.fill(self.batch, FILL) {
                        return Some(Err(e));
                    }
                    if self.batch.is_empty() {
                        self.done = true;
                        return None;
                    }
                }
                let i = self.batch.pos();
                if let Some(pre) = pre.as_deref_mut() {
                    if !(self.skip_from..self.skip_from + self.skip.len()).contains(&i) {
                        self.skip.clear();
                        self.skip_from = i;
                        pre.mark(self.batch, i..self.batch.len(), &mut self.skip);
                    }
                    let fate = self.skip[i - self.skip_from];
                    *passed = fate == PASSED;
                    if fate == DROP {
                        self.batch.advance();
                        self.left -= 1;
                        *skipped += 1;
                        if self.left == 0 {
                            return None;
                        }
                        continue;
                    }
                }
                self.left -= 1;
                self.batch.advance();
                return Some(Ok(self.batch.get(i)));
            },
            Source::Rows(it) => {
                let Some(r) = it.next() else {
                    self.done = true;
                    return None;
                };
                self.left -= 1;
                Some(r.map(|kv| {
                    let (k, v) = self.cur.insert(kv);
                    (&k[..], &v[..])
                }))
            }
        }
    }
}

/// A chunk of a secondary index's entries (or a bitmap heap scan's row keys: entries with
/// the data key as their value) as the rows they point at, in their order, fetched from the
/// store a batch at a time at the statement's snapshot (`Txn::store_reader`; None: an
/// index-only scan's, the entry key standing in for the row).
pub struct IndexChunk {
    pub entries: Box<dyn Iterator<Item = Result<(Vec<u8>, bytes::Bytes)>> + Send>,
    pub fetch: Option<(Arc<crate::storage::Store>, u64)>,
    buf: VecDeque<Result<(Vec<u8>, bytes::Bytes)>>,
    done: bool,
}

/// Index entries a chunk's rows are fetched for at a time.
const FETCH: usize = 256;

impl IndexChunk {
    pub fn new(entries: TxnScan, fetch: Option<(Arc<crate::storage::Store>, u64)>) -> IndexChunk {
        IndexChunk { entries: Box::new(entries), fetch, buf: VecDeque::new(), done: false }
    }

    /// The rows stored under `keys[run]` (in that order; the invisible ones left out).
    pub fn keyed(keys: Arc<[Vec<u8>]>, run: std::ops::Range<usize>, fetch: (Arc<crate::storage::Store>, u64)) -> IndexChunk {
        let entries = run.map(move |i| Ok((Vec::new(), bytes::Bytes::copy_from_slice(&keys[i]))));
        IndexChunk { entries: Box::new(entries), fetch: Some(fetch), buf: VecDeque::new(), done: false }
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
                            // (`keys` sorted and without duplicates: an entry's row is at its
                            // key's place, found by a search.)
                            for (_, d) in &ents {
                                if let Ok(j) = keys.binary_search(&&d[..])
                                    && let Some((_, Some(b))) = &got[j]
                                {
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
            // (The index entry: past the table and index ids, its key columns, up to the last
            // one read.)
            let mut row = vec![Value::Null; self.missing.len()];
            let mut pos = 8;
            for (col, ty, desc) in &cols[..ios_read(cols, need)] {
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
            Some(n) => crate::storage::row::decode_row_need_spare(data, n, self.extra.len())?,
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

    /// An index-only scan's row read from its entry in place (`IosRow`): the columns the scan
    /// reads, without the row's other columns.
    fn ios_row(&self, bytes: &[u8]) -> Result<IosRow> {
        let cols = self.ios.as_deref().unwrap_or(&[]);
        let mut vals = Vec::new();
        let mut pos = 8;
        for (col, ty, desc) in &cols[..ios_read(cols, self.need.as_ref())] {
            match crate::storage::keys::decode_index_value(bytes, &mut pos, ty, *desc) {
                Some(v) => vals.push((*col, v)),
                None => return Err(internal("an index entry that doesn't decode")),
            }
        }
        Ok(IosRow { width: self.missing.len(), vals })
    }

    /// One inline stored column (the caller excludes external and index-only rows).
    fn inline_col(&self, bytes: &[u8], col: usize) -> Result<Value> {
        let data = super::scan::row_data(bytes)?;
        let mut pos = 0;
        let stored = crate::storage::sst::get_varint(data, &mut pos)? as usize;
        if self.need.as_ref().is_some_and(|n| !n.get(col).copied().unwrap_or(false)) {
            return Ok(Value::Null);
        }
        if col >= stored {
            return Ok(self.missing.get(col).cloned().flatten().unwrap_or(Value::Null));
        }
        for _ in 0..col {
            crate::storage::row::skip_value(data, &mut pos)?;
        }
        crate::storage::row::decode_value(data, &mut pos)
    }

    /// `row` with the values of `extra` after its columns and the columns `drop` NULL
    /// (`done`: the filter is done with it, so nothing else reads them), when the kernels
    /// give them all; else the row as `row` makes it (the executor's thread evaluates them).
    /// `row` was decoded without the `raw` columns when `need_raw` is set: their distances
    /// come from the stored bytes (an out-of-line value's read through `lazy`'s reader).
    fn with_extra(&self, key: &[u8], bytes: &[u8], mut row: Row, done: bool, lazy: &LazyBuf) -> Result<Row> {
        let data = match self.need_raw {
            Some(_) => Some(super::scan::row_data(bytes)?),
            None => None,
        };
        // (The values go after the columns as they come; a row a kernel leaves to `eval`
        // goes back as it was.)
        let n = row.len();
        for k in &self.extra {
            let v = match (data, k.dist_col()) {
                (Some(data), Some((c, ..))) if self.raw.contains(&c) => match crate::storage::toast::has_external(bytes).then(|| crate::storage::toast::pointer_at(data, c)).flatten() {
                    Some(id) => {
                        let x = lazy.fetch(&crate::storage::toast::value_key(key, id))?;
                        crate::storage::row::ext_value(&x).and_then(|(kind, x)| k.dist_ext(kind, x))
                    }
                    None => k.dist_raw(data),
                },
                _ => k.run(&row[..n]),
            };
            match v {
                Some(v) => row.push(v),
                None if data.is_some() => return self.row_with(key, &lazy.splice(key, bytes, self.need.as_deref())?, self.need.as_ref()),
                None => {
                    row.truncate(n);
                    return Ok(row);
                }
            }
        }
        if done {
            for &c in &self.drop {
                if let Some(x) = row.get_mut(c) {
                    *x = Value::Null;
                }
            }
        }
        Ok(row)
    }
}

/// A worker's buffers for reading stored rows in place (`Lazy`), kept from row to row.
pub(super) struct LazyBuf {
    /// Per column walked, where its value starts (`u32::MAX`: not stored, added since).
    offs: Vec<u32>,
    /// Per column of the scan's row (and its key), the value once something read it (None
    /// inside: it doesn't decode; making the row reports why).
    cells: Vec<std::cell::OnceCell<Option<Value>>>,
    /// Per column walked, its exact number once read in place (`row::peek_num`; None
    /// inside: not one), so the aggregates and expressions reading it parse it once.
    nums: Vec<std::cell::OnceCell<Option<Option<(i128, u32)>>>>,
    /// The columns walked: up to the last one the scan reads.
    upto: usize,
    /// The worker's reader of the rows' out-of-line values (`Decode::toast`).
    reader: std::cell::RefCell<Option<crate::txn::toast::Reader>>,
}

impl LazyBuf {
    pub(super) fn new(d: &Decode) -> LazyBuf {
        let ncols = d.missing.len();
        let upto = match &d.need {
            Some(n) => n.iter().take(ncols).rposition(|b| *b).map_or(0, |i| i + 1),
            None => ncols,
        };
        LazyBuf {
            offs: vec![u32::MAX; upto],
            cells: (0..ncols + d.with_key as usize).map(|_| Default::default()).collect(),
            nums: (0..upto).map(|_| Default::default()).collect(),
            upto,
            reader: std::cell::RefCell::new(d.toast.clone().map(crate::txn::toast::Reader::new)),
        }
    }

    /// Out-of-line value `key` of the scan's rows (`Decode::toast`).
    pub(super) fn fetch(&self, key: &[u8]) -> Result<bytes::Bytes> {
        match &mut *self.reader.borrow_mut() {
            Some(r) => r.fetch(key),
            None => Err(internal("an out-of-line value read without its row's storage")),
        }
    }

    /// The stored row `bytes` with the out-of-line values of the columns `need` marks
    /// (None: all) put back, when it has some.
    pub(super) fn splice<'a>(&self, key: &[u8], bytes: &'a [u8], need: Option<&[bool]>) -> Result<std::borrow::Cow<'a, [u8]>> {
        if !crate::storage::toast::has_external(bytes) {
            return Ok(std::borrow::Cow::Borrowed(bytes));
        }
        let spliced = crate::storage::toast::splice(key, bytes, need, &mut |k| self.fetch(k))?;
        Ok(std::borrow::Cow::Owned(spliced))
    }

    /// The stored row `bytes` (under `key`) to be read in place: where its columns start,
    /// nothing decoded yet.
    pub(super) fn read<'a>(&'a mut self, d: &'a Decode, key: &'a [u8], bytes: &'a [u8]) -> Result<Lazy<'a>> {
        use crate::storage::{row::skip_value, sst::get_varint};
        let data = super::scan::row_data(bytes)?;
        let mut pos = 0;
        let n = get_varint(data, &mut pos)? as usize;
        for (i, off) in self.offs.iter_mut().enumerate() {
            if i < n {
                *off = pos as u32;
                skip_value(data, &mut pos)?;
            } else {
                *off = u32::MAX;
            }
        }
        for c in &mut self.cells {
            c.take();
        }
        for c in &mut self.nums {
            c.take();
        }
        let ext = crate::storage::toast::has_external(bytes);
        Ok(Lazy { d, key, data, ext, buf: self })
    }
}

/// A stored row read in place: a column is decoded when something first reads it, and
/// numbers are read without making values (`Cols::num`). What a filter rejects or a
/// partial aggregate takes is never made a row.
pub(super) struct Lazy<'a> {
    d: &'a Decode,
    key: &'a [u8],
    data: &'a [u8],
    /// The row points at out-of-line values (else no value is looked at for a pointer).
    ext: bool,
    buf: &'a mut LazyBuf,
}

impl Lazy<'_> {
    fn needed(&self, i: usize) -> bool {
        self.d.need.as_ref().is_none_or(|n| n.get(i).copied().unwrap_or(false))
    }

    /// Column `i` of the scan's row, as `Decode::row` makes it.
    fn decode(&self, i: usize) -> Result<Value> {
        let ncols = self.d.missing.len();
        if i == ncols {
            return Ok(Value::Bytea(Arc::from(self.key)));
        }
        if i >= self.buf.upto || !self.needed(i) {
            return Ok(Value::Null);
        }
        match self.buf.offs[i] {
            u32::MAX => Ok(self.d.missing[i].clone().unwrap_or(Value::Null)),
            off if !self.ext => crate::storage::row::decode_value(self.data, &mut (off as usize)),
            off => {
                let mut pos = off as usize;
                match crate::storage::toast::read_pointer(self.data, &mut pos)? {
                    Some((id, _)) => {
                        let v = self.buf.fetch(&crate::storage::toast::value_key(self.key, id))?;
                        crate::storage::row::decode_value(&v, &mut 0)
                    }
                    None => crate::storage::row::decode_value(self.data, &mut pos),
                }
            }
        }
    }

    /// The scan's row (`Decode::row`'s), with the values already read.
    pub(super) fn into_row(self) -> Result<Row> {
        let mut row = Vec::with_capacity(self.buf.cells.len() + self.d.extra.len());
        for i in 0..self.buf.cells.len() {
            let v = match self.buf.cells[i].take() {
                Some(Some(v)) => v,
                _ => self.decode(i)?,
            };
            row.push(v);
        }
        Ok(row)
    }
}

impl super::kernel::Cols for Lazy<'_> {
    fn col(&self, i: usize) -> Option<&Value> {
        self.buf.cells.get(i)?.get_or_init(|| self.decode(i).ok()).as_ref()
    }

    fn num(&self, i: usize) -> Option<Option<(i128, u32)>> {
        let cell = self.buf.cells.get(i)?;
        if cell.get().is_none()
            && i < self.buf.upto
            && self.needed(i)
            && self.buf.offs[i] != u32::MAX
            && let Some(r) = self.buf.nums[i]
                .get_or_init(|| crate::storage::row::peek_num(self.data, self.buf.offs[i] as usize))
        {
            return Some(*r);
        }
        let v = self.col(i)?;
        if v.is_null() { Some(None) } else { super::kernel::small_num(v).map(Some) }
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
    scan: Source,
    out: Sender<Msg>,
}

pub struct ParScan {
    /// Chunks not handed out yet, in key order.
    pending: VecDeque<Source>,
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
    /// A scan whose chunks run on the executor's thread, one when its rows are asked for
    /// (`start_inline`): no workers.
    inline: Option<Box<Inline>>,
    /// An unordered aggregation's workers' parts, sent as they end (`Spec::unordered`):
    /// read once every chunk is done.
    tail: Option<Receiver<Msg>>,
}

/// What an inline scan's chunks run with (a worker's own state, `work`).
struct Inline {
    /// The chunk being read, a slice at a time (`INLINE_SLICE`).
    cur: Option<Source>,
    d: Arc<Decode>,
    scratch: super::pipe::Scratch,
    lazy: LazyBuf,
    /// The chunk's rows read and not taken yet.
    batch: RowBatch,
}

thread_local! {
    /// Whether an aggregating scan with workers off runs inline (`without_serial_inline`).
    static SERIAL_INLINE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Whether an aggregating scan with max_parallel_workers_per_gather = 0 runs inline on this
/// thread.
pub(super) fn serial_inline() -> bool {
    SERIAL_INLINE.with(|c| c.get())
}

/// Run `f` with aggregating scans under max_parallel_workers_per_gather = 0 on the serial
/// path (tests compare the two).
#[doc(hidden)]
pub fn without_serial_inline<R>(f: impl FnOnce() -> R) -> R {
    let _g = crate::scoped::set(&SERIAL_INLINE, false, "parscan::SERIAL_INLINE");
    f()
}

/// The rows of an inline scan's chunk read at a time.
const INLINE_SLICE: usize = 1 << 16;

/// Scans run inline (`ParScan::start_inline`; for tests).
static INLINE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many scans the process has run inline.
pub fn inline_started() -> u64 {
    INLINE.load(std::sync::atomic::Ordering::Relaxed)
}

fn work(jobs: Arc<Mutex<Receiver<Job>>>, d: Arc<Decode>, stop: Arc<std::sync::atomic::AtomicBool>, tail: Option<Sender<Msg>>) {
    let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed);
    // (One for the worker's chunks: its lookups' cache lasts.)
    let mut scratch = super::pipe::Scratch::default();
    let mut lazy = LazyBuf::new(&d);
    let mut batch = RowBatch::default();
    // (An unordered aggregation's part, over all the worker's chunks: `Spec::unordered`.)
    let mut held = None;
    loop {
        let job = match jobs.lock() {
            Ok(rx) => rx.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else { break };
        if stopped() || !run_job(job, &d, &stopped, &mut scratch, &mut lazy, &mut batch, tail.is_some().then_some(&mut held)) {
            return;
        }
    }
    // (The chunks are all done: the part goes to the scan's tail.)
    if let (Some(tail), Some(mut pa)) = (tail, held)
        && let Some(p) = pa.take()
    {
        let _ = tail.send(Msg::Seg(super::partial::Seg::Part(p)));
    }
}

/// One chunk of a scan: its rows (or an aggregating scan's parts) sent to the job's `out`,
/// on a worker or (`ParScan::inline`) the executor's thread. False: nothing reads them
/// (the scan was dropped).
fn run_job<'d>(
    Job { mut scan, out }: Job,
    d: &'d Decode,
    stopped: &dyn Fn() -> bool,
    scratch: &mut super::pipe::Scratch,
    lazy: &mut LazyBuf,
    batch: &mut RowBatch,
    held: Option<&mut Option<super::partial::Partials<'d>>>,
) -> bool {
    batch.clear();
    run_rows(&mut Reader::new(&mut scan, batch, usize::MAX), out, d, stopped, scratch, lazy, held)
}

/// `run_job` over any run of rows (an inline scan's slice of a chunk).
#[allow(clippy::too_many_arguments)]
fn run_rows<'d>(
    scan: &mut Reader<'_>,
    out: Sender<Msg>,
    d: &'d Decode,
    stopped: &dyn Fn() -> bool,
    scratch: &mut super::pipe::Scratch,
    lazy: &mut LazyBuf,
    held: Option<&mut Option<super::partial::Partials<'d>>>,
) -> bool {
    {
        if let Some(spec) = &d.spec {
            return work_partial(scan, &out, d, spec, stopped, scratch, lazy, held);
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
        // Column-only projections can inspect the first key before making a row.
        let mut top_col = if d.ios.is_none() && d.need_raw.is_none() && d.raw.is_empty() && d.extra.iter().all(|k| matches!(k, K::Col(_))) {
            d.topk.and_then(|t| match d.extra.get(t.col) {
                Some(K::Col(col)) => Some(*col),
                _ => None,
            })
        } else {
            None
        };
        let mut consecutive_kept = 0;
        // (Rows a batch filter or probe drops before they're made: `Pre`.)
        let mut pre = match scan.fills() {
            true => Pre::new(d, None),
            false => None,
        };
        let mut dropped = 0u32;
        let mut passed = false;
        while let Some(r) = scan.next_row_with(pre.as_mut().filter(|p| !p.off), &mut dropped, &mut passed) {
            n = n.wrapping_add(1);
            if n % 1024 == 1023 && stopped() {
                return false;
            }
            read += 1 + std::mem::take(&mut dropped);
            let (key, bytes) = match r {
                Ok(kv) => kv,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            };
            // (Filtered in place when nothing is computed over the row: a row the filter
            // rejects is never made.)
            let in_place = d.ios.is_none() && d.extra.is_empty() && d.topk.is_none() && d.need_raw.is_none();
            let mut top_checked = false;
            let (row, done) = if let Some(col) = top_col {
                let made = if !d.filtered && col < d.missing.len() && !crate::storage::toast::has_external(bytes) {
                    d.inline_col(bytes, col).and_then(|value| {
                        top_checked = true;
                        if top.as_mut().is_some_and(|t| t.drops(&value)) {
                            consecutive_kept = 0;
                            skipped += 1;
                            return Ok(None);
                        }
                        Ok(Some((d.row_with(key, bytes, d.need.as_ref())?, true)))
                    })
                } else {
                    lazy.read(d, key, bytes).and_then(|lz| {
                        let done = match (&d.filter, d.filtered) {
                            (_, false) => true,
                            (Some(_), true) if passed => true,
                            (Some(f), true) => match f.passes(&lz) {
                                Some(true) => true,
                                Some(false) => return Ok(None),
                                None => false,
                            },
                            (None, true) => false,
                        };
                        if done
                            && let Some(key) = super::kernel::Cols::col(&lz, col)
                            && let Some(t) = &mut top
                        {
                            top_checked = true;
                            if t.drops(key) {
                                consecutive_kept = 0;
                                skipped += 1;
                                return Ok(None);
                            }
                        }
                        Ok(Some((lz.into_row()?, done)))
                    })
                };
                match made {
                    Ok(Some(r)) => r,
                    Ok(None) => continue,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            } else {
                match (&d.filter, in_place) {
                    (Some(f), true) => {
                        let made = lazy.read(d, key, bytes).and_then(|lz| match if passed { Some(true) } else { f.passes(&lz) } {
                            Some(false) => Ok(None),
                            pass => Ok(Some((lz.into_row()?, pass.is_some()))),
                        });
                        match made {
                            Ok(Some(r)) => r,
                            Ok(None) => continue,
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    }
                    _ => {
                        let mask = d.need_raw.as_ref().or(d.need.as_ref());
                        let made = lazy.splice(key, bytes, mask.map(|m| &m[..])).and_then(|b| d.row_with(key, &b, mask));
                        let row = match made {
                            Ok(row) => row,
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        };
                        let done = match (&d.filter, d.filtered) {
                            (_, false) => true,
                            (Some(_), true) if passed => true,
                            (Some(f), true) => match f.passes(&row) {
                                Some(true) => true,
                                Some(false) => continue,
                                None => false,
                            },
                            (None, true) => false,
                        };
                        (row, done)
                    }
                }
            };
            if top_checked {
                consecutive_kept += 1;
                if consecutive_kept >= BATCH {
                    // A full batch without a discard: normal decoding costs less
                    // when first keys tie or keep improving. Keep the same heap.
                    top_col = None;
                }
            }
            let row = if d.extra.is_empty() {
                row
            } else {
                match d.with_extra(key, bytes, row, done, lazy) {
                    Ok(row) => row,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            };
            // (A row the filter is done with is the sort's: counted, and with its key, kept
            // in the chunk's heap or dropped.)
            if done && !top_checked && let Some(t) = &mut top {
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
                super::pipe::run_stages(&d.stages, 0, row, scratch, &mut |r, lvl| {
                    batch.push((r.into_row(), lvl, true, std::mem::take(&mut read)));
                });
            }
            if batch.len() >= BATCH && out.send(Msg::Rows(std::mem::replace(&mut batch, Vec::with_capacity(BATCH)))).is_err() {
                // (The scan was dropped: nothing reads on.)
                return false;
            }
        }
        if !batch.is_empty() && out.send(Msg::Rows(batch)).is_err() {
            return false;
        }
        read += dropped;
        // (The chunk's lookups counted before its end is.)
        scratch.flush(&d.stages);
        let _ = out.send(match err {
            Some(e) => Msg::Err(e),
            None => Msg::Done(read, skipped),
        });
        true
    }
}

impl ParScan {
    /// Rows dropped after the last row returned (at the scan's end).
    pub fn trailing(&mut self) -> u32 {
        std::mem::take(&mut self.carry)
    }
}

/// Whether a row passes the scan's filter in a worker (None: the executor's thread decides).
fn filter_pass<R: super::kernel::Cols + ?Sized>(d: &Decode, row: &R) -> Option<bool> {
    match &d.filter {
        Some(f) => f.passes(row),
        None if d.filtered => None,
        None => Some(true),
    }
}

/// Parts sent at a time, at most (rows aggregated).
const PART_ROWS: usize = 1 << 16;

/// The groups of a part a worker keeps from chunk to chunk (`Spec::unordered`), at most.
const HELD_GROUPS: usize = 1 << 17;

/// A chunk aggregated (`partial`): false when nothing receives any more. An unordered
/// aggregation (`Spec::unordered`) keeps the worker's part from chunk to chunk (`held`), sent
/// when it holds `HELD_GROUPS` groups and at the scan's end (its rows read sent apart); rows
/// left to the serial path go without it.
#[allow(clippy::too_many_arguments)]
fn work_partial<'d>(
    scan: &mut Reader<'_>,
    out: &Sender<Msg>,
    d: &'d Decode,
    spec: &'d super::partial::Spec,
    stopped: &dyn Fn() -> bool,
    scratch: &mut super::pipe::Scratch,
    lazy: &mut LazyBuf,
    held: Option<&mut Option<super::partial::Partials<'d>>>,
) -> bool {
    match held {
        Some(h) => {
            let pa = h.take().unwrap_or_else(|| super::partial::Partials::new(spec));
            let (ok, pa) = partial_chunk(scan, out, d, spec, stopped, scratch, lazy, true, pa);
            *h = Some(pa);
            ok
        }
        None => partial_chunk(scan, out, d, spec, stopped, scratch, lazy, false, super::partial::Partials::new(spec)).0,
    }
}

/// `work_partial`'s chunk, its part owned while the rows go into it (`keep`: an unordered
/// aggregation's, kept after it): whether anything still receives, and the part.
#[allow(clippy::too_many_arguments)]
fn partial_chunk<'d>(
    scan: &mut Reader<'_>,
    out: &Sender<Msg>,
    d: &'d Decode,
    spec: &'d super::partial::Spec,
    stopped: &dyn Fn() -> bool,
    scratch: &mut super::pipe::Scratch,
    lazy: &mut LazyBuf,
    keep: bool,
    mut pa: super::partial::Partials<'d>,
) -> (bool, super::partial::Partials<'d>) {
    use super::partial::{Partials, Seg};
    let full = |pa: &Partials| if keep { pa.groups() >= HELD_GROUPS } else { pa.rows() >= PART_ROWS };
    // (Rows read and not counted in a part yet.)
    let mut read = 0u64;
    let send_part = |pa: &mut Partials, read: &mut u64| {
        if keep {
            // (The rows read so far go now, in the chunk's order, in a part of no groups:
            // counted where the serial scan counts them, an error's too.)
            if pa.groups() >= HELD_GROUPS
                && let Some(p) = pa.take()
                && out.send(Msg::Seg(Seg::Part(p))).is_err()
            {
                return false;
            }
            let read = std::mem::take(read);
            return read == 0
                || out.send(Msg::Seg(Seg::Part(super::partial::Part { groups: super::partial::Groups::Values(Vec::new()), seq: Vec::new(), read, rows: 0 }))).is_ok();
        }
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
    // (A fill's rows a batch at a time as columns, when the filter and aggregates have
    // batch kernels, or below hash joins the bottom one's probe: `vector`. A row they can't
    // do goes through the row path in its place.)
    let mut pre = match scan.fills() {
        true => Pre::new(d, Some(spec)),
        false => None,
    };
    let mut pre_off = false;
    loop {
        if pre_off {
            pre = None;
        }
        if let Some(pre) = &mut pre {
            let run = match scan.next_run() {
                Some(Ok(run)) => run,
                Some(Err(e)) => return (send_part(&mut pa, &mut read) && out.send(Msg::Err(e)).is_ok(), pa),
                None => break,
            };
            n += run.len();
            if n % 4096 < run.len() && stopped() {
                return (false, pa);
            }
            pre.decode(scan.batch, run.clone());
            let pa_run = run.len();
            // (The whole run at once where the batch decides every row; else row by row.)
            if pre.aggregate(&mut pa) {
                read += pa_run as u64;
                pre.worth(pa_run, pa_run as u64);
                RUN_ROWS.fetch_add(pa_run as u64, std::sync::atomic::Ordering::Relaxed);
                if full(&pa) && !send_part(&mut pa, &mut read) {
                    return (false, pa);
                }
                continue;
            }
            pre.eval_nums();
            let mut taken = 0;
            for (r, i) in run.enumerate() {
                use super::vector::Tri;
                read += 1;
                if pre.fate(r) == DROP {
                    taken += 1;
                    continue;
                }
                let aggregated = !pre.odd[r]
                    && pre.tri[r] == Tri::T
                    && pre.plan.probe.is_none()
                    && pa.batch_row(&pre.plan, &pre.cols, &pre.nums, r);
                if aggregated {
                    taken += 1;
                } else {
                    let (k, v) = scan.batch.get(i);
                    if let Err(e) = partial_row(d, &mut pa, &mut serial, scratch, lazy, top, k, v) {
                        return (send_part(&mut pa, &mut read) && out.send(Msg::Err(e)).is_ok(), pa);
                    }
                }
                if !serial.is_empty() {
                    if !send_part(&mut pa, &mut read) || out.send(Msg::Seg(Seg::Rows(std::mem::take(&mut serial)))).is_err() {
                        return (false, pa);
                    }
                } else if full(&pa) && !send_part(&mut pa, &mut read) {
                    return (false, pa);
                }
            }
            if !pre.worth(pa_run, taken) {
                pre_off = true;
            }
            continue;
        }
        let Some(r) = scan.next_row() else { break };
        n += 1;
        if n.is_multiple_of(4096) && stopped() {
            return (false, pa);
        }
        read += 1;
        let (k, v) = match r {
            Ok(e) => e,
            Err(e) => return (send_part(&mut pa, &mut read) && out.send(Msg::Err(e)).is_ok(), pa),
        };
        if let Err(e) = partial_row(d, &mut pa, &mut serial, scratch, lazy, top, k, v) {
            return (send_part(&mut pa, &mut read) && out.send(Msg::Err(e)).is_ok(), pa);
        }
        if !serial.is_empty() {
            if !send_part(&mut pa, &mut read) || out.send(Msg::Seg(Seg::Rows(std::mem::take(&mut serial)))).is_err() {
                return (false, pa);
            }
        } else if full(&pa) && !send_part(&mut pa, &mut read) {
            return (false, pa);
        }
    }
    scratch.flush(&d.stages);
    (send_part(&mut pa, &mut read) && out.send(Msg::Done(0, 0)).is_ok(), pa)
}

/// Whether aggregating scans read fills as column batches (`vector`; tests compare both).
fn vectors() -> bool {
    NO_VECTORS.load(std::sync::atomic::Ordering::Relaxed) == 0
}

/// Callers of `without_vectors` running (process-wide: the workers read it too; both paths
/// give the same results, so other statements running meanwhile see no difference).
static NO_VECTORS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Runs `f` with aggregating scans reading row by row (tests: the column path against the
/// row path).
#[doc(hidden)]
pub fn without_vectors<R>(f: impl FnOnce() -> R) -> R {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            NO_VECTORS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    NO_VECTORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _off = Off;
    f()
}

/// How many of an index-only scan's columns are decoded: up to the last one `need` marks
/// (a count's none; the entry's values come in their order).
fn ios_read(cols: &[(usize, crate::types::DataType, bool)], need: Option<&Arc<[bool]>>) -> usize {
    match need {
        Some(n) => cols.iter().rposition(|c| n.get(c.0).copied().unwrap_or(true)).map_or(0, |i| i + 1),
        None => cols.len(),
    }
}

static NULL: Value = Value::Null;

/// An index-only scan's row as its entry gives it (`Decode::ios_row`): the columns decoded,
/// every other one NULL, made into a row only when the part can't take it.
struct IosRow {
    width: usize,
    vals: Vec<(usize, Value)>,
}

impl super::kernel::Cols for IosRow {
    fn col(&self, i: usize) -> Option<&Value> {
        (i < self.width).then(|| self.vals.iter().find(|v| v.0 == i).map_or(&NULL, |v| &v.1))
    }
}

impl IosRow {
    fn into_row(self) -> Row {
        let mut row = vec![Value::Null; self.width];
        for (i, v) in self.vals {
            row[i] = v;
        }
        row
    }
}

/// Rows aggregated through column batches (`vector`; tests: which path ran).
pub static VECTOR_ROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Of them, rows aggregated a whole batch at a time (`Partials::batch_run`; tests).
pub static RUN_ROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One stored row of an aggregating scan through the row path: read in place, filtered,
/// aggregated or queued in `serial` (with the joins' rows it makes), in order.
#[allow(clippy::too_many_arguments)]
fn partial_row(
    d: &Decode,
    pa: &mut super::partial::Partials,
    serial: &mut Vec<(Row, u8, bool)>,
    scratch: &mut super::pipe::Scratch,
    lazy: &mut LazyBuf,
    top: u8,
    k: &[u8],
    v: &[u8],
) -> Result<()> {
    // (Read in place: a row the filter rejects or the part takes is never made.)
    let row = match &d.ios {
        None => lazy.read(d, k, v).and_then(|lazy| {
            let pass = filter_pass(d, &lazy);
            match pass {
                Some(false) => Ok(None),
                Some(true) if d.stages.is_empty() && pa.row(&lazy) => Ok(None),
                _ => lazy.into_row().map(|row| Some((row, pass))),
            }
        }),
        // (An index entry read in place likewise, without the key column a row can carry.)
        Some(_) if !d.with_key => d.ios_row(v).map(|ios| {
            let pass = filter_pass(d, &ios);
            match pass {
                Some(false) => None,
                Some(true) if d.stages.is_empty() && pa.row(&ios) => None,
                _ => Some((ios.into_row(), pass)),
            }
        }),
        Some(_) => d.row(k, v).map(|row| {
            let pass = filter_pass(d, &row);
            Some((row, pass))
        }),
    };
    // (Whether the part was offered the row read in place already.)
    let offered = d.ios.is_none() || !d.with_key;
    let Some((row, pass)) = row? else { return Ok(()) };
    match pass {
        Some(false) => {}
        None => serial.push((row, 0, false)),
        Some(true) if d.stages.is_empty() => {
            // (One read in place was offered to the part already.)
            if offered || !pa.row(&row) {
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
    Ok(())
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
    pub fn start(scans: Vec<Source>, workers: usize, pool: usize, d: Decode) -> Option<ParScan> {
        let reserved = reserve(workers, pool);
        let workers = reserved;
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let d = Arc::new(d);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tail_tx, tail) = match d.spec.as_ref().is_some_and(|s| s.unordered) {
            true => {
                let (tx, rx) = channel::<Msg>();
                (Some(tx), Some(rx))
            }
            false => (None, None),
        };
        let workers = (0..workers)
            .filter_map(|i| {
                let (rx, d, stop, tail) = (rx.clone(), d.clone(), stop.clone(), tail_tx.clone());
                std::thread::Builder::new()
                    .name(format!("roll-scan-{i}"))
                    .spawn(move || work(rx, d, stop, tail))
                    .ok()
            })
            .collect::<Vec<_>>();
        drop(tail_tx);
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
            inline: None,
            tail,
        };
        let ahead = workers.len() * AHEAD + 1;
        p.workers = workers;
        for _ in 0..ahead {
            p.hand_out();
        }
        Some(p)
    }

    /// The scan run on the executor's thread, a chunk at a time as its rows are asked for:
    /// what workers do with each row (decoding only what is read, filtering by kernels,
    /// aggregating, computing), without threads, for a table too small for them to pay.
    pub fn start_inline(scans: Vec<Source>, d: Decode) -> ParScan {
        INLINE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lazy = LazyBuf::new(&d);
        ParScan {
            pending: scans.into(),
            results: VecDeque::new(),
            jobs: None,
            batch: Vec::new().into_iter(),
            seg: Vec::new().into_iter(),
            workers: Vec::new(),
            carry: 0,
            skipped: 0,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            inline: Some(Box::new(Inline { cur: None, d: Arc::new(d), scratch: Default::default(), lazy, batch: RowBatch::default() })),
            tail: None,
        }
    }

    /// How many chunks to cut a scan into for `workers` threads.
    pub fn chunks(workers: usize) -> usize {
        workers * CHUNKS_PER_WORKER
    }

    fn hand_out(&mut self) {
        // (An inline scan runs its chunks as they are reached: `next_piece`.)
        if self.inline.is_some() {
            return;
        }
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
            if self.results.is_empty()
                && let Some(i) = &mut self.inline
            {
                // (A slice of the chunk at a time: what it holds and how long the executor's
                // thread goes without checking for a cancel stay bounded.)
                let Inline { cur, d, scratch, lazy, batch } = &mut **i;
                if cur.is_none() {
                    *cur = self.pending.pop_front();
                    batch.clear();
                }
                if let Some(c) = cur {
                    let (out, rx) = channel();
                    self.results.push_back(rx);
                    let mut rd = Reader::new(c, batch, INLINE_SLICE);
                    run_rows(&mut rd, out, d, &|| false, scratch, lazy, None);
                    if rd.done {
                        *cur = None;
                    }
                }
            }
            let Some(rx) = self.results.front() else {
                // (Every chunk done: the workers, told no more come, send their parts.)
                if let Some(t) = &self.tail {
                    self.jobs = None;
                    match t.recv() {
                        Ok(Msg::Seg(Seg::Part(p))) => return Ok(Some(Piece::Part(p))),
                        Ok(_) => return Err(internal("a parallel scan worker sent rows after its chunks")),
                        Err(_) => self.tail = None,
                    }
                }
                return Ok(None);
            };
            match rx.recv() {
                Ok(Msg::Rows(rows)) => self.batch = rows.into_iter(),
                Ok(Msg::Seg(Seg::Part(p))) => return Ok(Some(Piece::Part(p))),
                Ok(Msg::Seg(Seg::Rows(rows))) => self.seg = rows.into_iter(),
                Ok(Msg::Err(e)) => {
                    self.results.clear();
                    self.pending.clear();
                    if let Some(i) = &mut self.inline {
                        i.cur = None;
                    }
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
        self.tail = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
            ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_key_pruning_preserves_two_key_tuplesort() {
        use crate::exec::pgsort::{Bounded, Cmp, tuplesort};
        use crate::types::Numeric;

        let numbers = |values: &[&str]| values.iter().map(|v| Value::numeric(v.parse::<Numeric>().unwrap())).collect::<Vec<_>>();
        let keys = [
            vec![Value::Null, Value::Int4(-1), Value::Int4(0), Value::Int4(1)],
            vec![Value::Null, Value::Float8(f64::NAN), Value::Float8(-0.0), Value::Float8(0.0), Value::Float8(-1.0), Value::Float8(1.0)],
            numbers(&["-1", "0", "0.00", "3", "3.00", "NaN"]),
            // Every first key ties; late rows win by the second key.
            vec![Value::Null],
            vec![Value::Float8(f64::NAN)],
            vec![Value::Float8(-0.0), Value::Float8(0.0)],
            numbers(&["3", "3.00"]),
        ];
        let mut skipped = 0;
        for values in keys {
            let items: Vec<_> = (0..257).map(|i| (values[(i * 37) % values.len()].clone(), i)).collect();
            for desc in [false, true] {
                for nulls_first in [false, true] {
                    for bound in [1, 3, 7, 17, usize::MAX] {
                        let t = crate::exec::TopK { col: 0, desc, nulls_first, k: bound, keep_ties: true };
                        for chunk in [1, 7, 19, 128, 257] {
                            let mut f = |a: &(Value, usize), b: &(Value, usize)| Ok(t.cmp(&a.0, &b.0).then_with(|| b.1.cmp(&a.1)));
                            let want = tuplesort(items.clone(), Some(bound), &mut Cmp::new(&mut f));
                            let mut sort = Bounded::new(bound);
                            let mut cmp = Cmp::new(&mut f);
                            for part in items.chunks(chunk) {
                                let mut top = Top { t, seen: 0, heap: std::collections::BinaryHeap::new() };
                                for row in part {
                                    if top.drops(&row.0) {
                                        skipped += 1;
                                        sort.skip(1);
                                    } else {
                                        sort.put(row.clone(), &mut cmp);
                                    }
                                }
                            }
                            let got: Vec<_> = sort.finish(&mut cmp).into_iter().map(|r| r.1).collect();
                            assert_eq!(got, want.into_iter().map(|r| r.1).collect::<Vec<_>>(), "{t:?}, chunks of {chunk}, keys {values:?}");
                        }
                    }
                }
            }
        }
        assert!(skipped > 0, "the comparison exercised first-key pruning");
    }

    struct Rows(std::ops::Range<u32>);

    impl FillRows for Rows {
        fn fill(&mut self, batch: &mut RowBatch, max: usize) -> Result<()> {
            for i in self.0.by_ref().take(max) {
                batch.push_copy(&i.to_be_bytes(), &(i * 3).to_be_bytes());
            }
            Ok(())
        }
    }

    #[test]
    fn row_sources_preserve_rows_across_slices() {
        for len in [0, 1, FILL - 1, FILL, FILL + 1, INLINE_SLICE - 1, INLINE_SLICE, INLINE_SLICE + 1] {
            for slice in [1, FILL - 1, FILL, FILL + 1, INLINE_SLICE] {
                let rows = (0..len as u32).map(|i| Ok((i.to_be_bytes().to_vec(), bytes::Bytes::copy_from_slice(&(i * 3).to_be_bytes()))));
                for mut source in [fill_source(Rows(0..len as u32), false), Source::Rows(Box::new(rows))] {
                    let mut batch = RowBatch::default();
                    let mut seen = 0u32;
                    loop {
                        let mut reader = Reader::new(&mut source, &mut batch, slice);
                        while let Some(row) = reader.next_row() {
                            let (key, value) = row.unwrap();
                            assert_eq!(key, seen.to_be_bytes());
                            assert_eq!(value, (seen * 3).to_be_bytes());
                            seen += 1;
                        }
                        if reader.done {
                            break;
                        }
                    }
                    assert_eq!(seen as usize, len, "{len} rows in slices of {slice}");
                }
            }
        }
    }
}
