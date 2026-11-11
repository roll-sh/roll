//! The order a PostgreSQL tuplesort (`utils/sort/tuplesort.c`) returns tuples in, as the
//! ivfflat build (by list) and scan (by distance) sort theirs: equal keys come out as its
//! quicksort leaves them, and once the tuples outgrow the sort's memory, as its external sort
//! does: runs cut where its memory accounting (USEMEM / LACKMEM, `grow_memtuples`, the tape
//! buffers `inittapestate` reserves) cuts them, each quicksorted, then merged a run per tape
//! through its binary heap, in balanced passes while there are more runs than tapes.
//!
//! Only the order is modelled: tuples stay in memory.

use crate::exec::pgsort::{Cmp, pg_qsort};
use std::cmp::Ordering;
use std::collections::VecDeque;

/// sizeof(SortTuple).
const SORT_TUPLE: i64 = 24;
/// INITIAL_MEMTUPSIZE: Max(1024, ALLOCSET_SEPARATE_THRESHOLD / sizeof(SortTuple) + 1).
const INITIAL_MEMTUPSIZE: usize = 1024;
/// TAPE_BUFFER_OVERHEAD and MERGE_BUFFER_SIZE.
const TAPE_BUFFER_OVERHEAD: i64 = 8192;
const MERGE_BUFFER_SIZE: i64 = 8192 * 32;
const MINORDER: i64 = 6;
const MAXORDER: i64 = 500;

/// GetMemoryChunkSpace of a memtuples array of `n` (a chunk of its own: the size and the
/// chunk header).
fn array_space(n: usize) -> i64 {
    n as i64 * SORT_TUPLE + 8
}

/// PostgreSQL's quicksort (`qsort_tuple`) of `v` by `cmp`.
fn qsort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    let mut f = |a: &T, b: &T| Ok(cmp(a, b));
    pg_qsort(v, &mut Cmp::new(&mut f));
}

/// `tuplesort_merge_order`.
fn merge_order(allowed: i64) -> usize {
    (allowed / (2 * TAPE_BUFFER_OVERHEAD + MERGE_BUFFER_SIZE)).clamp(MINORDER, MAXORDER) as usize
}

/// A sort's state across batches (`tuplesort_reset` keeps the memtuples array when it is
/// still of its initial size, and then doesn't count it again).
pub(super) struct Sorter {
    allowed: i64,
    memtupsize: usize,
    /// Whether the next batch counts its array (`tuplesort_begin_batch` allocating it).
    count_array: bool,
}

/// The tuplesort's state while tuples are put (`TSS_INITIAL` / `TSS_BUILDRUNS`).
struct Put<T> {
    avail: i64,
    memtupsize: usize,
    grow: bool,
    tuple_mem: i64,
    building_runs: bool,
    max_tapes: usize,
    cur: Vec<T>,
    runs: Vec<Vec<T>>,
}

impl Sorter {
    /// `tuplesort_begin_heap` with `work_mem` KB (`tuplesort_begin_common`: at least 64).
    pub(super) fn new(work_mem: i64) -> Sorter {
        Sorter { allowed: work_mem.max(64) * 1024, memtupsize: INITIAL_MEMTUPSIZE, count_array: true }
    }

    /// `tuplesort_reset`, for another batch.
    pub(super) fn reset(&mut self) {
        // (A new array is counted; the one kept isn't: availMem starts at allowedMem.)
        self.count_array = self.memtupsize != INITIAL_MEMTUPSIZE;
        self.memtupsize = INITIAL_MEMTUPSIZE;
    }

    /// Put `items` (each taking `tuplen` of the sort's memory), perform the sort and read it
    /// all: the order PostgreSQL returns them in, by `cmp`.
    pub(super) fn sort<T>(&mut self, items: Vec<T>, tuplen: impl Fn(&T) -> i64, mut cmp: impl FnMut(&T, &T) -> Ordering) -> Vec<T> {
        // tuplesort_begin_batch
        let mut p = Put {
            avail: self.allowed - if self.count_array { array_space(self.memtupsize) } else { 0 },
            memtupsize: self.memtupsize,
            grow: true,
            tuple_mem: 0,
            building_runs: false,
            max_tapes: 0,
            cur: Vec::new(),
            runs: Vec::new(),
        };
        self.count_array = false;
        let c: &mut dyn FnMut(&T, &T) -> Ordering = &mut cmp;
        for t in items {
            let len = tuplen(&t);
            p.put(t, len, self.allowed, c);
        }
        // tuplesort_performsort
        if !p.building_runs {
            self.memtupsize = p.memtupsize;
            qsort(&mut p.cur, c);
            return p.cur;
        }
        p.dump(true, c);
        let runs = std::mem::take(&mut p.runs);
        // (mergeruns: the heap's array has a slot per output tape.)
        self.memtupsize = runs.len().min(p.max_tapes);
        self.count_array = true;
        merge_runs(runs, p.max_tapes, c)
    }
}

impl<T> Put<T> {
    /// `tuplesort_puttuple_common`.
    fn put(&mut self, t: T, tuplen: i64, allowed: i64, c: &mut dyn FnMut(&T, &T) -> Ordering) {
        self.avail -= tuplen;
        self.tuple_mem += tuplen;
        if self.building_runs {
            self.cur.push(t);
            self.dump(false, c);
            return;
        }
        if self.cur.len() >= self.memtupsize - 1 {
            self.grow_memtuples(allowed);
        }
        self.cur.push(t);
        if self.cur.len() < self.memtupsize && self.avail >= 0 {
            return;
        }
        // inittapes / inittapestate
        self.max_tapes = merge_order(allowed);
        let tape_space = self.max_tapes as i64 * TAPE_BUFFER_OVERHEAD;
        if tape_space + array_space(self.memtupsize) < allowed {
            self.avail -= tape_space;
        }
        self.building_runs = true;
        self.dump(false, c);
    }

    /// `grow_memtuples`.
    fn grow_memtuples(&mut self, allowed: i64) {
        if !self.grow {
            return;
        }
        let memtupsize = self.memtupsize as i64;
        let now_used = allowed - self.avail;
        let mut newsize: i64;
        if now_used <= self.avail {
            // We've used no more than half of allowedMem; double our usage.
            if memtupsize < i32::MAX as i64 / 2 {
                newsize = memtupsize * 2;
            } else {
                newsize = i32::MAX as i64;
                self.grow = false;
            }
        } else {
            // The last increment: as much as we safely can.
            let ratio = allowed as f64 / now_used as f64;
            newsize = if (memtupsize as f64 * ratio) < i32::MAX as f64 { (memtupsize as f64 * ratio) as i64 } else { i32::MAX as i64 };
            self.grow = false;
        }
        // Must enlarge array by at least one element, else report failure
        if newsize <= memtupsize || self.avail < (newsize - memtupsize) * SORT_TUPLE {
            self.grow = false;
            return;
        }
        let limit = (usize::MAX / 2) as i64 / SORT_TUPLE;
        if newsize >= limit {
            newsize = limit;
            self.grow = false;
        }
        self.avail += array_space(self.memtupsize);
        self.memtupsize = newsize as usize;
        self.avail -= array_space(self.memtupsize);
    }

    /// `dumptuples`: the tuples in memory as a sorted run, when they no longer fit (or at the
    /// end, `all`).
    fn dump(&mut self, all: bool, c: &mut dyn FnMut(&T, &T) -> Ordering) {
        if self.cur.len() < self.memtupsize && self.avail >= 0 && !all {
            return;
        }
        if self.cur.is_empty() && !self.runs.is_empty() {
            return;
        }
        qsort(&mut self.cur, c);
        self.runs.push(std::mem::take(&mut self.cur));
        self.avail += self.tuple_mem;
        self.tuple_mem = 0;
    }
}

/// `mergeruns` (and the final on-the-fly merge): runs go to tapes round robin past
/// `max_tapes`; while there are more runs than tapes, a balanced pass merges one run of each
/// tape at a time onto new tapes.
fn merge_runs<T>(runs: Vec<Vec<T>>, max_tapes: usize, c: &mut dyn FnMut(&T, &T) -> Ordering) -> Vec<T> {
    let mut tapes: Vec<VecDeque<Vec<T>>> = Vec::new();
    let mut nruns = 0usize;
    let place = |tapes: &mut Vec<VecDeque<Vec<T>>>, nruns: &mut usize, run: Vec<T>| {
        // selectnewtape
        if tapes.len() < max_tapes {
            tapes.push(VecDeque::from([run]));
        } else {
            let n = tapes.len();
            tapes[*nruns % n].push_back(run);
        }
        *nruns += 1;
    };
    for r in runs {
        place(&mut tapes, &mut nruns, r);
    }
    loop {
        let mut input = std::mem::take(&mut tapes);
        let mut in_runs = nruns;
        nruns = 0;
        if in_runs <= input.len() {
            return merge_one(&mut input, in_runs, c);
        }
        while in_runs > 0 {
            let active = input.len().min(in_runs);
            let merged = merge_one(&mut input, active, c);
            in_runs -= active;
            place(&mut tapes, &mut nruns, merged);
        }
    }
}

/// `beginmerge` + `mergeonerun`: the next run of each of the first `active` tapes merged
/// through the binary heap (`tuplesort_heap_insert` / `_replace_top` / `_delete_top`).
fn merge_one<T>(tapes: &mut [VecDeque<Vec<T>>], active: usize, c: &mut dyn FnMut(&T, &T) -> Ordering) -> Vec<T> {
    let mut src: Vec<std::vec::IntoIter<T>> = tapes[..active].iter_mut().map(|t| t.pop_front().unwrap_or_default().into_iter()).collect();
    let mut out = Vec::with_capacity(src.iter().map(|s| s.len()).sum());
    let mut heap: Vec<(T, usize)> = Vec::with_capacity(active);
    for (i, s) in src.iter_mut().enumerate() {
        if let Some(t) = s.next() {
            heap_insert(&mut heap, (t, i), c);
        }
    }
    while !heap.is_empty() {
        let i = heap[0].1;
        let next = src[i].next();
        let top = match next {
            Some(t) => heap_replace_top(&mut heap, (t, i), c),
            None => heap_delete_top(&mut heap, c),
        };
        out.push(top.0);
    }
    out
}

/// `tuplesort_heap_insert`: sift up.
fn heap_insert<T>(heap: &mut Vec<(T, usize)>, t: (T, usize), c: &mut dyn FnMut(&T, &T) -> Ordering) {
    heap.push(t);
    let mut j = heap.len() - 1;
    while j > 0 {
        let i = (j - 1) >> 1;
        if c(&heap[j].0, &heap[i].0) != Ordering::Less {
            break;
        }
        heap.swap(i, j);
        j = i;
    }
}

/// `tuplesort_heap_replace_top`: `t` replaces the top, sifting down; the old top returned.
fn heap_replace_top<T>(heap: &mut [(T, usize)], t: (T, usize), c: &mut dyn FnMut(&T, &T) -> Ordering) -> (T, usize) {
    let top = std::mem::replace(&mut heap[0], t);
    let n = heap.len();
    // i is where the new tuple is (the "hole")
    let mut i = 0;
    loop {
        let mut j = 2 * i + 1;
        if j >= n {
            break;
        }
        if j + 1 < n && c(&heap[j].0, &heap[j + 1].0) == Ordering::Greater {
            j += 1;
        }
        if c(&heap[i].0, &heap[j].0) != Ordering::Greater {
            break;
        }
        heap.swap(i, j);
        i = j;
    }
    top
}

/// `tuplesort_heap_delete_top`: the last tuple replaces the top.
fn heap_delete_top<T>(heap: &mut Vec<(T, usize)>, c: &mut dyn FnMut(&T, &T) -> Ordering) -> (T, usize) {
    let last = heap.pop().expect("heap is not empty");
    if heap.is_empty() {
        return last;
    }
    heap_replace_top(heap, last, c)
}
