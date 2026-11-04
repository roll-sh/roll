//! A Sort past work_mem: tuplesort's external sort. Rows are counted as tuplesort counts its
//! tuples (`workmem::SortMem`); where tuplesort dumps a run to tape, the rows so far are
//! sorted (as its quicksort orders them) and written to a temporary file as a run, and the
//! runs are merged as mergeruns merges them: runs dealt to at most merge-order tapes, merge
//! passes while a tape holds more than one, then a final merge as rows are read, through
//! tuplesort's heap (so equal keys come out in its order). EXPLAIN ANALYZE gets
//! tuplesort_get_stats' method and space.

use super::spill::{RowReader, TempFile};
use super::workmem::{self, BLCKSZ, SortMem};
use super::{ExecCtx, Row};
use crate::error::Result;
use crate::sql::expr::{Expr, eval};
use crate::sql::plan::SortKey;
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::collections::VecDeque;

/// tuplesort_get_stats' sortMethod.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMethod {
    #[default]
    None,
    TopN,
    Quicksort,
    ExternalSort,
    ExternalMerge,
}

impl SortMethod {
    pub fn name(self) -> &'static str {
        match self {
            SortMethod::None => "still in progress",
            SortMethod::TopN => "top-N heapsort",
            SortMethod::Quicksort => "quicksort",
            SortMethod::ExternalSort => "external sort",
            SortMethod::ExternalMerge => "external merge",
        }
    }

    pub fn on_disk(self) -> bool {
        matches!(self, SortMethod::ExternalSort | SortMethod::ExternalMerge)
    }
}

/// Runs on tapes: each tape's runs as positions in the file, in order.
struct Tapes {
    file: TempFile,
    tapes: Vec<VecDeque<(u64, u64)>>,
    /// nOutputRuns, and the merge order (maxTapes).
    runs: usize,
    max: usize,
    /// PostgreSQL's bytes on each tape.
    pg: Vec<i64>,
}

impl Tapes {
    /// selectnewtape: a new tape while fewer than the merge order, else the next in turn.
    fn select(&mut self) -> usize {
        let t = if self.tapes.len() < self.max {
            self.tapes.push(VecDeque::new());
            self.pg.push(0);
            self.tapes.len() - 1
        } else {
            self.runs % self.tapes.len()
        };
        self.runs += 1;
        t
    }
}

pub struct ExtSort {
    /// The keys as compared: columns of the rows held (narrowed rows' places; keys that
    /// aren't columns are evaluated as rows come and appended to them).
    keys: Vec<SortKey>,
    cols: Vec<usize>,
    /// The keys as given (over the input's rows), the columns a held row has before the
    /// appended keys, and the input's column count.
    given: Vec<SortKey>,
    width: usize,
    full: usize,
    /// The sorted tuple's columns as PostgreSQL has them: the input's, then the keys that
    /// aren't columns (resjunk); and the ones read above (the others count as NULLs).
    types: Vec<DataType>,
    mask: Option<Vec<bool>>,
    /// tuplesort_begin_datum (PostgreSQL's sort input is one column, the one read): that
    /// column; its values aren't counted in memory.
    datum: Option<usize>,
    mem: SortMem,
    rows: Vec<Row>,
    /// Once the first run is written, the rows of the next ones packed as they come.
    packed: Option<Packed>,
    /// PostgreSQL's bytes on tape of `rows` (writetup_heap / writetup_datum).
    run_pg: i64,
    tapes: Option<Tapes>,
    /// TUPLESORT_RANDOMACCESS: the runs end up merged onto one tape ("external sort").
    random: bool,
}

/// What a sort's rows come out of.
pub enum Sorted {
    Mem(std::vec::IntoIter<Row>),
    Merge(Box<Merge>),
}

impl Sorted {
    pub fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        match self {
            Sorted::Mem(it) => Ok(it.next()),
            Sorted::Merge(m) => m.next(ctx),
        }
    }
}

impl Default for Sorted {
    fn default() -> Self {
        Sorted::Mem(Vec::new().into_iter())
    }
}

impl ExtSort {
    /// A sort of rows of `types` by `keys`; `narrow`: the rows are held narrowed (and its
    /// keys over them).
    pub(super) fn new(
        keys: &[SortKey],
        types: Vec<DataType>,
        mask: Option<Vec<bool>>,
        narrow: Option<&(super::nodes::Narrow, Vec<SortKey>)>,
        random: bool,
        ctx: &ExecCtx,
    ) -> ExtSort {
        let full = types.len();
        let width = narrow.map_or(full, |(n, _)| n.kept());
        let held = narrow.map_or(keys, |(_, k)| &k[..]);
        let mut extra = 0;
        let (keys2, cols): (Vec<SortKey>, Vec<usize>) = held
            .iter()
            .map(|k| {
                let c = match &k.expr {
                    Expr::Col(i, _) => *i,
                    _ => {
                        extra += 1;
                        width + extra - 1
                    }
                };
                (SortKey { expr: Expr::Col(c, k.expr.ty()), ..k.clone() }, c)
            })
            .collect();
        // (A key that isn't a column is one of the sorted tuple's (resjunk) columns too.)
        let mut types = types;
        let mut mask = mask;
        for k in held.iter().filter(|k| !matches!(k.expr, Expr::Col(..))) {
            types.push(k.expr.ty());
            if let Some(m) = &mut mask {
                m.resize(full, false);
                m.push(true);
            }
        }
        ExtSort {
            keys: keys2,
            cols,
            given: keys.to_vec(),
            width,
            full,
            datum: {
                // (The columns PostgreSQL's sort tuple has: those read, and the appended keys.)
                let cols: Vec<usize> = (0..types.len())
                    .filter(|&i| i >= full || mask.as_ref().is_none_or(|m| m.get(i).copied().unwrap_or(false)))
                    .collect();
                if let [c] = cols[..] { Some(c) } else { None }
            },
            mask,
            types,
            mem: SortMem::new(workmem::work_mem(ctx)),
            rows: Vec::new(),
            packed: None,
            run_pg: 4,
            tapes: None,
            random,
        }
    }

    /// An input row (of the input's columns), narrowed by `narrow` to be held.
    pub(super) fn put(&mut self, mut row: Row, narrow: Option<&super::nodes::Narrow>, ctx: &ExecCtx) -> Result<()> {
        for (k, &c) in self.given.iter().zip(&self.cols) {
            if c >= self.width {
                let v = eval(&k.expr, &row, ctx)?;
                row.push(v);
            }
        }
        let (tuplen, tape) = self.sizes(&row, ctx);
        if let Some(n) = narrow {
            let keys = row.split_off(self.full);
            row = n.shrink(row);
            row.extend(keys);
        }
        match &mut self.packed {
            Some(p) => p.push(row, &self.cols),
            None => self.rows.push(row),
        }
        self.run_pg += tape;
        if self.mem.put(tuplen) {
            self.dump(ctx)?;
        }
        Ok(())
    }

    /// What tuplesort counts of a tuple (`row`: the input's columns and the appended keys) in
    /// memory and on tape (writetup_heap / writetup_datum).
    fn sizes(&self, row: &[Value], ctx: &ExecCtx) -> (i64, i64) {
        let trail = if self.random { 4 } else { 0 };
        if let Some(c) = self.datum {
            let v = &row[c];
            let ty = &self.types[c];
            let n = if v.is_null() {
                0
            } else if crate::catalog::relsize::typlen(ty) > 0 && crate::catalog::relsize::typlen(ty) <= 8 {
                8
            } else {
                let text = |v: &Value, t: &DataType| crate::types::text::format_value(v, t, ctx.fmt());
                crate::catalog::relsize::datum_width(v, ty, &text) as i64
            };
            return (0, 4 + n + trail);
        }
        let len = workmem::minimal_tuple_len_of(row, &self.types, self.mask.as_deref(), ctx);
        (workmem::maxalign(len), len - workmem::MINIMAL_TUPLE_DATA_OFFSET + 4 + trail)
    }

    /// dumptuples: the rows so far sorted and written as the next run.
    fn dump(&mut self, ctx: &ExecCtx) -> Result<()> {
        if let Some(mut p) = self.packed.take() {
            let r = self.dump_packed(&mut p, ctx);
            self.packed = Some(p);
            return r;
        }
        let rows = std::mem::take(&mut self.rows);
        if rows.is_empty() && self.tapes.is_some() {
            return Ok(());
        }
        let rows = self.sort_run(rows, ctx)?;
        // (The next runs' rows are packed as they come.)
        self.packed = Some(Packed::default());
        if self.tapes.is_none() {
            let max = self.mem.tapes.unwrap_or(6);
            self.tapes = Some(Tapes { file: TempFile::create(ctx)?, tapes: Vec::new(), runs: 0, max, pg: Vec::new() });
        }
        let pg = std::mem::replace(&mut self.run_pg, 4);
        let t = self.tapes.as_mut().unwrap();
        let tape = t.select();
        let start = t.file.pos();
        for r in &rows {
            t.file.write_row(r)?;
        }
        t.tapes[tape].push_back((start, t.file.pos()));
        t.pg[tape] += pg;
        // (Full blocks are written as runs build; partial ones as the tapes are read.)
        let full: i64 = t.pg.iter().map(|b| b / workmem::TAPE_BLOCK_DATA).sum();
        t.file.pg_size((full * BLCKSZ) as u64)
    }

    /// A packed run (`Packed`) sorted as `sort_run` sorts rows, and written.
    fn dump_packed(&mut self, p: &mut Packed, ctx: &ExecCtx) -> Result<()> {
        let n = p.ends.len();
        if n == 0 {
            return Ok(());
        }
        let order = p.order(&self.keys, ctx)?;
        let pg = std::mem::replace(&mut self.run_pg, 4);
        let t = self.tapes.as_mut().expect("a run written before");
        let tape = t.select();
        let start = t.file.pos();
        for i in order {
            t.file.append(p.row(i))?;
        }
        p.clear();
        t.tapes[tape].push_back((start, t.file.pos()));
        t.pg[tape] += pg;
        let full: i64 = t.pg.iter().map(|b| b / workmem::TAPE_BLOCK_DATA).sum();
        t.file.pg_size((full * BLCKSZ) as u64)
    }

    /// One run's rows in tuplesort's order.
    fn sort_run(&self, rows: Vec<Row>, ctx: &ExecCtx) -> Result<Vec<Row>> {
        super::nodes::sort_rows(rows, &self.keys, ctx)
    }

    /// tuplesort_performsort, and the stats EXPLAIN ANALYZE shows: (method, kB).
    pub fn finish(mut self, ctx: &ExecCtx) -> Result<(Sorted, SortMethod, u64)> {
        let Some(_) = &self.tapes else {
            let kb = (self.mem.used() as u64).div_ceil(1024);
            let mut rows = super::nodes::sort_rows(std::mem::take(&mut self.rows), &self.keys, ctx)?;
            if self.cols.iter().any(|&c| c >= self.width) {
                for r in &mut rows {
                    r.truncate(self.width);
                }
            }
            return Ok((Sorted::Mem(rows.into_iter()), SortMethod::Quicksort, kb));
        };
        self.dump(ctx)?;
        let t = self.tapes.take().unwrap();
        let blocks = workmem::tape_blocks(&t.pg);
        t.file.pg_size((blocks * BLCKSZ) as u64)?;
        let kb = (blocks * BLCKSZ) as u64 / 1024;
        let method = if self.random { SortMethod::ExternalSort } else { SortMethod::ExternalMerge };
        let width = self.width;
        let keys = self.keys;
        let m = merge_runs(t, keys, width, self.random, ctx)?;
        Ok((Sorted::Merge(Box::new(m)), method, kb))
    }
}

/// mergeruns: merge passes while some tape holds more than one run (or, with random
/// access, until one run is left), then the final merge's rows as they're read.
fn merge_runs(mut t: Tapes, keys: Vec<SortKey>, width: usize, random: bool, ctx: &ExecCtx) -> Result<Merge> {
    loop {
        let inputs = std::mem::take(&mut t.tapes);
        let nruns: usize = inputs.iter().map(VecDeque::len).sum();
        let ntapes = inputs.len();
        t.runs = 0;
        t.pg.clear();
        if !random && nruns <= ntapes {
            let mut srcs = Vec::new();
            for mut q in inputs {
                if let Some((s, e)) = q.pop_front() {
                    srcs.push(t.file.reader(s, e)?);
                }
            }
            return Merge::begin(t.file, srcs, keys, width, ctx);
        }
        // Merges of the next run of each tape (of as many tapes as runs remain), each onto
        // the next output tape, until the input runs are used up.
        let mut inputs = inputs;
        let mut left = nruns;
        while left > 0 {
            let active = ntapes.min(left);
            let mut srcs = Vec::new();
            for q in inputs.iter_mut().take(active) {
                if let Some((s, e)) = q.pop_front() {
                    srcs.push(t.file.reader(s, e)?);
                }
            }
            left -= srcs.len();
            let out = t.select();
            let start = t.file.pos();
            let mut m = Merge::begin_borrowed(srcs, &keys, ctx)?;
            while let Some(r) = m.next_raw(&keys, ctx)? {
                ctx.check()?;
                t.file.write_row(&r)?;
            }
            t.tapes[out].push_back((start, t.file.pos()));
        }
        if t.tapes.iter().map(VecDeque::len).sum::<usize>() <= 1 && random {
            let mut srcs = Vec::new();
            if let Some((s, e)) = t.tapes.first_mut().and_then(VecDeque::pop_front) {
                srcs.push(t.file.reader(s, e)?);
            }
            return Merge::begin(t.file, srcs, keys, width, ctx);
        }
    }
}

/// A merge of sorted runs through tuplesort's heap (tuplesort_heap_insert / replace_top /
/// delete_top): (row, source) entries, the top the smallest.
pub struct Merge {
    heap: Vec<(Row, usize)>,
    srcs: Vec<RowReader>,
    keys: Vec<SortKey>,
    width: usize,
    /// Keeps the runs' file open while they're read.
    _file: Option<TempFile>,
}

fn cmp(a: &Row, b: &Row, keys: &[SortKey], ctx: &ExecCtx) -> Result<Ordering> {
    for k in keys {
        let Expr::Col(c, _) = k.expr else { unreachable!() };
        match super::nodes::compare_values(&a[c], &b[c], k, ctx)? {
            Ordering::Equal => {}
            o => return Ok(o),
        }
    }
    Ok(Ordering::Equal)
}

impl Merge {
    fn begin(file: TempFile, srcs: Vec<RowReader>, keys: Vec<SortKey>, width: usize, ctx: &ExecCtx) -> Result<Merge> {
        let mut m = Merge::begin_borrowed(srcs, &keys, ctx)?;
        m.keys = keys;
        m.width = width;
        m._file = Some(file);
        Ok(m)
    }

    /// beginmerge: each source's first row into the heap.
    fn begin_borrowed(mut srcs: Vec<RowReader>, keys: &[SortKey], ctx: &ExecCtx) -> Result<Merge> {
        let mut heap: Vec<(Row, usize)> = Vec::with_capacity(srcs.len());
        for (i, s) in srcs.iter_mut().enumerate() {
            if let Some(r) = s.read_row()? {
                // tuplesort_heap_insert.
                let mut j = heap.len();
                heap.push((r, i));
                while j > 0 {
                    let p = (j - 1) >> 1;
                    if cmp(&heap[j].0, &heap[p].0, keys, ctx)? != Ordering::Less {
                        break;
                    }
                    heap.swap(j, p);
                    j = p;
                }
            }
        }
        Ok(Merge { heap, srcs, keys: Vec::new(), width: usize::MAX, _file: None })
    }

    /// The next row (the input's columns only).
    pub fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let keys = std::mem::take(&mut self.keys);
        let r = self.next_raw(&keys, ctx);
        self.keys = keys;
        Ok(r?.map(|mut r| {
            r.truncate(self.width);
            r
        }))
    }

    fn next_raw(&mut self, keys: &[SortKey], ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.heap.is_empty() {
            return Ok(None);
        }
        let src = self.heap[0].1;
        let next = match self.srcs[src].read_row()? {
            Some(r) => Some((r, src)),
            // tuplesort_heap_delete_top: the last entry replaces the top.
            None => {
                let last = self.heap.pop().unwrap();
                if self.heap.is_empty() {
                    return Ok(Some(last.0));
                }
                Some(last)
            }
        };
        let top = self.replace_top(next.unwrap(), keys, ctx)?;
        Ok(Some(top))
    }

    /// tuplesort_heap_replace_top: `t` sifted down from the top; the old top returned.
    fn replace_top(&mut self, t: (Row, usize), keys: &[SortKey], ctx: &ExecCtx) -> Result<Row> {
        let n = self.heap.len();
        let mut i = 0;
        loop {
            let mut j = 2 * i + 1;
            if j >= n {
                break;
            }
            if j + 1 < n && cmp(&self.heap[j].0, &self.heap[j + 1].0, keys, ctx)? == Ordering::Greater {
                j += 1;
            }
            if cmp(&t.0, &self.heap[j].0, keys, ctx)? != Ordering::Greater {
                break;
            }
            self.heap.swap(i, j);
            i = j;
        }
        // (The old top sank to `i` through the swaps: `t` takes its place.)
        Ok(std::mem::replace(&mut self.heap[i], t).0)
    }
}

/// tuplesort_get_stats of a bounded sort (a LIMIT above) that read `n` rows and kept `kept`
/// (the first `bound`): the bounded heap once more than twice the bound came, else a
/// quicksort of all of them; its tuples counted by their aset.c chunks.
pub fn bounded_stats(
    kept: &[Row],
    n: usize,
    bound: usize,
    keys: &[SortKey],
    types: &[DataType],
    ctx: &ExecCtx,
) -> (SortMethod, u64) {
    let held = n.min(2 * bound + 1);
    let mut size = 1024usize;
    for c in 0..held {
        if c + 1 >= size {
            size *= 2;
        }
    }
    // (A text first key: its value copied per tuple as well, as measured on PostgreSQL 18.6.)
    let text_key = keys.first().is_some_and(|k| {
        matches!(k.expr.ty(), DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_))
    });
    let per = |r: &Row| -> i64 {
        let mut b = workmem::aset_chunk(workmem::minimal_tuple_len(r, types, ctx));
        if text_key && let Ok(Value::Text(s)) = eval(&keys[0].expr, r, ctx) {
            b += workmem::maxalign(s.len() as i64 + 4) + 8;
        }
        b
    };
    let sum: i64 = kept.iter().map(per).sum();
    let array = workmem::aset_chunk(size as i64 * 24);
    let (method, tuples) = if n > 2 * bound {
        (SortMethod::TopN, sum)
    } else {
        let avg = if kept.is_empty() { 0 } else { sum / kept.len() as i64 };
        (SortMethod::Quicksort, avg * n as i64)
    };
    (method, ((array + tuples) as u64).div_ceil(1024))
}


/// A run's rows packed as they come in (`spill::put_row`, one buffer) and their keys' values
/// beside them: the run is sorted by the keys and written in its order without reaching rows
/// spread over the heap (each a vector of values) again, which was most of a spilling sort's
/// time, and holds a fraction of their memory.
#[derive(Default)]
struct Packed {
    bytes: Vec<u8>,
    /// Each row's end in `bytes`.
    ends: Vec<usize>,
    /// The keys' values, row by row.
    keys: Vec<Value>,
}

impl Packed {
    fn push(&mut self, row: Row, cols: &[usize]) {
        self.keys.extend(cols.iter().map(|&c| row[c].clone()));
        super::spill::put_row(&mut self.bytes, &row);
        self.ends.push(self.bytes.len());
    }

    fn row(&self, i: usize) -> &[u8] {
        &self.bytes[if i == 0 { 0 } else { self.ends[i - 1] }..self.ends[i]]
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
        self.keys.clear();
    }

    /// The rows' order as `nodes::sort_rows` would sort them (tuplesort's quicksort over
    /// the rows in the order they came).
    fn order(&self, keys: &[SortKey], ctx: &ExecCtx) -> Result<Vec<usize>> {
        let (n, nk) = (self.ends.len(), keys.len());
        let key = |i: usize, k: usize| &self.keys[i * nk + k];
        if let Some(o) = super::fastsort::order_with(n, keys, key, |a, b| self.row(a) == self.row(b), None) {
            return Ok(o);
        }
        let mut f = |a: &usize, b: &usize| -> Result<Ordering> {
            for (k, sk) in keys.iter().enumerate() {
                match super::nodes::compare_values(key(*a, k), key(*b, k), sk, ctx)? {
                    Ordering::Equal => {}
                    o => return Ok(o),
                }
            }
            Ok(Ordering::Equal)
        };
        let mut cmp = super::pgsort::Cmp::new(&mut f);
        let sorted = super::pgsort::tuplesort((0..n).collect(), None, &mut cmp);
        match cmp.err {
            Some(e) => Err(e),
            None => Ok(sorted),
        }
    }
}
