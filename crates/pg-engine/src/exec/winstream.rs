//! A window's partition past work_mem, run as nodeWindowAgg.c runs one: its rows read in
//! (spooled) as far as the current row's functions and frame need, the rows no read pointer
//! can reach again freed after each row (tuplestore_trim), and the partition's rows in a
//! temporary file once PostgreSQL's count of them (`workmem::StoreMem`) passes work_mem,
//! read back by a cursor per pointer. Each row goes up with its results as it is computed.
//!
//! `window::WindowNode` holds a partition in memory and computes it whole (`run_windows`)
//! while it fits; past work_mem, and when its calls are ones this runs (`Spec::of`), the
//! partition goes on here. The spooling and freeing follow `window::count_store`'s model of
//! PostgreSQL's read pointers row by row, so the store spills where PostgreSQL's would and
//! its temporary file has PostgreSQL's size.

use super::agg::{AggState, feed, new_state, peek};
use super::spill::{RowReader, TempAcct, TempCleanup, TempFile, put_row};
use super::workmem::{BLCKSZ, MINIMAL_TUPLE_DATA_OFFSET, StoreMem, minimal_tuple_len_cols};
use super::{ExecCtx, Row, eval_all};
use crate::error::{Error, Result, code};
use crate::sql::expr::eval;
use crate::sql::plan::{FrameBound, FrameExclude, FrameUnit, SortKey, WinFunc, WinKind, WindowCall};
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::rc::Rc;

/// What a call computes for the current row.
#[derive(Clone, Copy, PartialEq)]
enum Func {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    /// lag (negative) or lead by a constant offset: the row that many after the current.
    Offset(i64),
    First,
    Last,
    /// nth_value's constant position (from 1).
    Nth(usize),
    Agg,
}

/// The frame's end for the current row (its start is the partition's first row).
#[derive(Clone, Copy, PartialEq)]
enum End {
    /// ROWS ... CURRENT ROW: the current row.
    Row,
    /// RANGE / GROUPS ... CURRENT ROW, ordered: the current row's last peer.
    Peers,
    /// UNBOUNDED FOLLOWING, or RANGE / GROUPS ... CURRENT ROW without ORDER BY.
    Part,
}

/// The calls of one window, as this runs them.
pub(super) struct Spec {
    funcs: Vec<Func>,
    end: End,
    /// The frame is RANGE / GROUPS (a tail pointer for CURRENT ROW).
    ranged: bool,
    ordered: bool,
    aggs: bool,
    /// Trailing length words on tape (the store was made EXEC_FLAG_BACKWARD).
    pub backward: bool,
    /// The current row's peer group's end / start is kept (a function or the frame's tail
    /// pointer reads it).
    peer_end: bool,
    peer_start: bool,
    /// Some function reads the partition's size, or the frame its end: it is read whole.
    all: bool,
}

impl Spec {
    /// The calls (one window) when this runs each of them: frames from the partition's start
    /// to the current row (or its peers) or the partition's end with no exclusion, ranking
    /// functions, lag / lead / nth_value with constant offsets and no IGNORE NULLS,
    /// aggregates without DISTINCT or ORDER BY. None: the partition is computed whole.
    pub(super) fn of(calls: &[WindowCall]) -> Option<Spec> {
        let c0 = calls.first()?;
        let frameless = |c: &WindowCall| {
            matches!(
                c.func,
                WinFunc::Builtin(
                    WinKind::RowNumber | WinKind::Rank | WinKind::DenseRank | WinKind::PercentRank | WinKind::CumeDist | WinKind::Ntile
                )
            )
        };
        // (optimize_window_clauses: functions that ignore the frame get ROWS UNBOUNDED
        // PRECEDING; as `count_store`.)
        let all_frameless = calls.iter().all(frameless);
        let f = &c0.frame;
        let (unit, end) = if all_frameless {
            (FrameUnit::Rows, FrameBound::CurrentRow)
        } else {
            if !matches!(f.start, FrameBound::UnboundedPreceding) || f.exclude != FrameExclude::NoOthers {
                return None;
            }
            (f.unit, f.end.clone())
        };
        let ordered = !c0.order_by.is_empty();
        let ranged = unit != FrameUnit::Rows;
        let end = match end {
            FrameBound::UnboundedFollowing => End::Part,
            FrameBound::CurrentRow if !ranged => End::Row,
            FrameBound::CurrentRow if ordered => End::Peers,
            FrameBound::CurrentRow => End::Part,
            _ => return None,
        };
        let mut funcs = Vec::with_capacity(calls.len());
        for c in calls {
            funcs.push(match &c.func {
                WinFunc::Builtin(k) => {
                    if c.ignore_nulls {
                        return None;
                    }
                    match k {
                        WinKind::RowNumber => Func::RowNumber,
                        WinKind::Rank => Func::Rank,
                        WinKind::DenseRank => Func::DenseRank,
                        WinKind::PercentRank => Func::PercentRank,
                        WinKind::CumeDist => Func::CumeDist,
                        WinKind::Ntile => Func::Ntile,
                        WinKind::Lag | WinKind::Lead => {
                            let k0 = const_arg(c, 1, Some(1))?;
                            Func::Offset(if *k == WinKind::Lag { k0.checked_neg()? } else { k0 })
                        }
                        WinKind::FirstValue => Func::First,
                        WinKind::LastValue => Func::Last,
                        WinKind::NthValue => Func::Nth(usize::try_from(const_arg(c, 1, None)?).ok().filter(|n| *n > 0)?),
                    }
                }
                WinFunc::Agg(a) => {
                    if a.distinct || !a.order_by.is_empty() || a.def.kind.is_ordered() {
                        return None;
                    }
                    Func::Agg
                }
            });
        }
        let aggs = funcs.contains(&Func::Agg);
        let builtins = funcs.iter().any(|f| *f != Func::Agg);
        // (A RANGE / GROUPS frame ending at the current row has a tail pointer, kept at the
        // peer group's end whatever the functions read: ExecWindowAgg's update_frametailpos.)
        let peer_end = ordered && (funcs.contains(&Func::CumeDist) || end == End::Peers);
        let peer_start = ordered && funcs.iter().any(|f| matches!(f, Func::Rank | Func::DenseRank | Func::PercentRank));
        let all = funcs.iter().any(|f| matches!(f, Func::PercentRank | Func::CumeDist | Func::Ntile))
            || end == End::Part && funcs.iter().any(|f| matches!(f, Func::Last | Func::Agg));
        Some(Spec { funcs, end, ranged, ordered, aggs, backward: builtins, peer_end, peer_start, all })
    }
}

/// A lag / lead / nth_value argument that is a constant integer (`window::const_offset`).
fn const_arg(call: &WindowCall, arg: usize, default: Option<i64>) -> Option<i64> {
    match call.args.get(arg) {
        None => default,
        Some(crate::sql::expr::Expr::Const(v, _)) => v.as_i64(),
        Some(_) => None,
    }
}

/// The rows of the current partition after the ones read so far: those read before the
/// window went on here, then the input's until the next partition.
pub(super) trait Source {
    fn pull(&mut self, ctx: &ExecCtx) -> Result<Option<Row>>;
}

/// The bytes a tuple of t_len `tlen` takes on tape (writetup_heap).
fn tape_len(tlen: i64, backward: bool) -> u64 {
    (tlen - MINIMAL_TUPLE_DATA_OFFSET + 4 + if backward { 4 } else { 0 }) as u64
}

/// The partition's rows on disk: those from `lo` on, in order, and a reader per cursor.
struct Disk {
    file: TempFile,
    acct: Option<TempAcct>,
    pg_bytes: u64,
    lo: usize,
    cursors: Vec<Cursor>,
    buf: Vec<u8>,
}

/// A read pointer into the file: the row it read last (at `pos - 1`), and where the next is.
#[derive(Default)]
struct Cursor {
    pos: usize,
    off: u64,
    rd: Option<RowReader>,
    row: Row,
}

// The cursors: the current row, the peer group's end, the aggregates' next row, then one per
// call (lag / lead's row, last_value's, nth_value's).
const CUR: usize = 0;
const PEER: usize = 1;
const AGG: usize = 2;

/// One partition, streamed.
pub(super) struct Part {
    /// The rows from `deleted` to `spooled` while in memory, with their t_lens.
    rows: VecDeque<Row>,
    tlens: VecDeque<i64>,
    deleted: usize,
    /// tuplestore_trim's moves out of the pointer array (`WinStore::partition`'s base).
    base: usize,
    spooled: usize,
    /// The partition's rows are all read (`spooled` is its size).
    ended: bool,
    disk: Option<Disk>,
    /// The current row.
    r: usize,
    /// The current row's peer group [gs, ge) (ge: as far as found), the next group's
    /// first row's ORDER BY values once found, and dense_rank's count.
    gs: usize,
    ge: usize,
    gkeys: Vec<Value>,
    next_keys: Option<Vec<Value>>,
    dense: i64,
    /// The aggregates' states (by call), the rows they have read, their results for that.
    states: Vec<Option<AggState>>,
    fed: usize,
    results: Vec<Option<(usize, Value)>>,
    /// first_value's and nth_value's results, once read (by call).
    fixed: Vec<Option<Value>>,
    /// Each builtin's mark (by call).
    marks: Vec<usize>,
    cleanup: Rc<TempCleanup>,
}

/// What the window keeps of its input's rows for PostgreSQL's tuple sizes.
pub(super) struct Shape<'a> {
    pub types: &'a [DataType],
    pub keep: Option<&'a [bool]>,
}

impl Part {
    pub(super) fn new(spec: &Spec, ctx: &ExecCtx) -> Part {
        let n = spec.funcs.len();
        Part {
            rows: VecDeque::new(),
            tlens: VecDeque::new(),
            deleted: 0,
            base: 0,
            spooled: 0,
            ended: false,
            disk: None,
            r: 0,
            gs: 0,
            ge: 0,
            gkeys: Vec::new(),
            next_keys: None,
            dense: 0,
            states: (0..n).map(|_| None).collect(),
            fed: 0,
            results: vec![None; n],
            fixed: vec![None; n],
            marks: vec![0; n],
            cleanup: ctx.temp_cleanup.clone(),
        }
    }

    /// Read rows in until `want` are (or the partition ends).
    fn spool(&mut self, want: usize, src: &mut dyn Source, mem: &mut StoreMem, shape: &Shape, backward: bool, ctx: &ExecCtx) -> Result<()> {
        while self.spooled < want && !self.ended {
            let Some(row) = src.pull(ctx)? else {
                self.ended = true;
                break;
            };
            let tlen = minimal_tuple_len_cols(&row, shape.types, shape.keep, ctx);
            if mem.put(tlen) {
                // (dumptuples: the rows held so far go to the file, then this one.)
                for &t in &self.tlens {
                    mem.dumped(t);
                }
                mem.dumped(tlen);
                self.dump(ctx, backward)?;
            }
            match &mut self.disk {
                Some(d) => {
                    d.buf.clear();
                    put_row(&mut d.buf, &row);
                    d.file.append(&d.buf)?;
                    d.pg_bytes += tape_len(tlen, backward);
                    if let Some(a) = &d.acct {
                        a.pg_size(d.pg_bytes / BLCKSZ as u64 * BLCKSZ as u64)?;
                    }
                }
                None => {
                    self.rows.push_back(row);
                    self.tlens.push_back(tlen);
                }
            }
            self.spooled += 1;
        }
        Ok(())
    }

    /// The rows in memory go to a new file; PostgreSQL's file holds them too.
    fn dump(&mut self, ctx: &ExecCtx, backward: bool) -> Result<()> {
        let mut d = Disk {
            file: TempFile::create_raw(ctx)?,
            acct: Some(TempAcct::new(ctx)),
            pg_bytes: 0,
            lo: self.deleted,
            cursors: Vec::new(),
            buf: Vec::new(),
        };
        for (row, &t) in std::mem::take(&mut self.rows).iter().zip(&self.tlens) {
            d.buf.clear();
            put_row(&mut d.buf, row);
            d.file.append(&d.buf)?;
            d.pg_bytes += tape_len(t, backward);
        }
        if let Some(a) = &d.acct {
            a.pg_size(d.pg_bytes / BLCKSZ as u64 * BLCKSZ as u64)?;
        }
        self.tlens.clear();
        self.disk = Some(d);
        Ok(())
    }

    /// The row at position `p` (spooled, not freed), through cursor `cur` when on disk
    /// (each cursor's positions only go forward).
    fn at(&mut self, cur: usize, p: usize) -> Result<&Row> {
        debug_assert!(p >= self.deleted && p < self.spooled);
        let Some(d) = self.disk.as_mut() else {
            return Ok(&self.rows[p - self.deleted]);
        };
        while d.cursors.len() <= cur {
            d.cursors.push(Cursor { pos: d.lo, ..Default::default() });
        }
        let c = &mut d.cursors[cur];
        debug_assert!(p + 1 >= c.pos, "a cursor went back");
        while c.pos <= p {
            if let Some(rd) = c.rd.as_mut()
                && let Some(row) = rd.read_row()?
            {
                c.off = rd.offset();
                c.row = row;
                c.pos += 1;
                continue;
            }
            c.rd = Some(d.file.reader(c.off, d.file.pos())?);
        }
        Ok(&d.cursors[cur].row)
    }

    /// The current row's peer group's end: rows read in while they are its peers.
    #[allow(clippy::too_many_arguments)]
    fn find_peer_end(&mut self, keys: &[SortKey], src: &mut dyn Source, mem: &mut StoreMem, shape: &Shape, backward: bool, ctx: &ExecCtx) -> Result<()> {
        loop {
            self.spool(self.ge + 1, src, mem, shape, backward, ctx)?;
            if self.ge >= self.spooled {
                return Ok(());
            }
            let row = self.at(PEER, self.ge)?;
            let k: Vec<Value> = keys.iter().map(|k| eval(&k.expr, row, ctx)).collect::<Result<_>>()?;
            if cmp_keys(&k, &self.gkeys, keys, ctx)? != Ordering::Equal {
                self.next_keys = Some(k);
                return Ok(());
            }
            self.ge += 1;
        }
    }

    /// The current row with its calls' results, then the next one current; None once the
    /// partition's rows are all out.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn next(
        &mut self,
        spec: &Spec,
        calls: &[WindowCall],
        src: &mut dyn Source,
        mem: &mut StoreMem,
        shape: &Shape,
        ctx: &ExecCtx,
    ) -> Result<Option<Row>> {
        let r = self.r;
        let bw = spec.backward;
        self.spool(r + 1, src, mem, shape, bw, ctx)?;
        if r >= self.spooled {
            return Ok(None);
        }
        let keys = &calls[0].order_by;
        // The current row's peers.
        if spec.ordered && (spec.peer_end || spec.peer_start) && (r == 0 || r >= self.ge) {
            let k = match self.next_keys.take() {
                Some(k) if spec.peer_end => k,
                _ => {
                    let row = self.at(CUR, r)?;
                    keys.iter().map(|k| eval(&k.expr, row, ctx)).collect::<Result<_>>()?
                }
            };
            let new = r == 0 || spec.peer_end || cmp_keys(&k, &self.gkeys, keys, ctx)? != Ordering::Equal;
            if new {
                self.gs = r;
                self.dense += 1;
                self.gkeys = k;
            }
            self.ge = r + 1;
            if spec.peer_end {
                self.find_peer_end(keys, src, mem, shape, bw, ctx)?;
            }
        }
        // (A function reading the partition's size, or a frame to its end, reads it all.)
        if spec.all {
            self.spool(usize::MAX, src, mem, shape, bw, ctx)?;
        }
        // The frame's end for this row ([0, e)); to the partition's end only once read
        // (nth_value's position is spooled to, and nothing else reads it then).
        let e = match spec.end {
            End::Row => r + 1,
            End::Peers => self.ge,
            End::Part if self.ended => self.spooled,
            End::Part => usize::MAX,
        };
        let mut sp = r + 1;
        if spec.ranged && spec.end == End::Peers || spec.aggs {
            sp = sp.max(e.saturating_add(1));
        }
        for f in &spec.funcs {
            match *f {
                Func::Offset(d) => sp = sp.max((r as i64).saturating_add(d).saturating_add(1).max(0) as usize),
                Func::Nth(n) => sp = sp.max(n),
                Func::Last => sp = sp.max(if spec.end == End::Row { r + 1 } else { e.saturating_add(1) }),
                Func::First => sp = sp.max(1),
                _ => {}
            }
        }
        self.spool(sp, src, mem, shape, bw, ctx)?;
        let m = self.ended.then_some(self.spooled);
        // The results.
        let mut out: Vec<Value> = Vec::with_capacity(spec.funcs.len());
        if spec.aggs && self.fed < e {
            while self.fed < e {
                let p = self.fed;
                self.at(AGG, p)?;
                let row = match &self.disk {
                    Some(d) => &d.cursors[AGG].row,
                    None => &self.rows[p - self.deleted],
                };
                for ((f, c), st) in spec.funcs.iter().zip(calls).zip(self.states.iter_mut()) {
                    if *f != Func::Agg {
                        continue;
                    }
                    let WinFunc::Agg(agg) = &c.func else { unreachable!() };
                    if st.is_none() {
                        *st = Some(new_state(agg, eval_all(&agg.direct, &[], ctx)?)?);
                    }
                    feed(agg, st.as_mut().unwrap(), row, ctx)?;
                }
                self.fed += 1;
            }
        }
        for (ci, (f, c)) in spec.funcs.iter().zip(calls).enumerate() {
            let size = || m.expect("the partition read whole");
            let v = match *f {
                Func::RowNumber => Value::Int8(r as i64 + 1),
                Func::Rank => Value::Int8(if spec.ordered { self.gs as i64 + 1 } else { 1 }),
                Func::DenseRank => Value::Int8(if spec.ordered { self.dense } else { 1 }),
                Func::PercentRank => {
                    let gs = if spec.ordered { self.gs } else { 0 };
                    Value::Float8(if size() <= 1 { 0.0 } else { gs as f64 / (size() - 1) as f64 })
                }
                Func::CumeDist => Value::Float8(if spec.ordered { self.ge } else { size() } as f64 / size() as f64),
                Func::Ntile => {
                    let k = eval(&c.args[0], self.at(CUR, r)?, ctx)?;
                    if k.is_null() {
                        Value::Null
                    } else {
                        let k = k.as_i64().unwrap_or(0);
                        if k <= 0 {
                            return Err(Error::new(code::INVALID_ARGUMENT_FOR_NTILE, "argument of ntile must be greater than zero"));
                        }
                        let (m, k) = (size(), k as usize);
                        let (base, extra) = (m / k, m % k);
                        let bucket = if r < extra * (base + 1) { r / (base + 1) } else { extra + (r - extra * (base + 1)) / base.max(1) };
                        Value::Int8(bucket as i64 + 1)
                    }
                }
                Func::Offset(d) => {
                    let t = (r as i64).saturating_add(d);
                    if t >= 0 && (t as usize) < self.spooled {
                        let row = self.at(3 + ci, t as usize)?;
                        eval(&c.args[0], row, ctx)?
                    } else {
                        match c.args.get(2) {
                            Some(dflt) => eval(dflt, self.at(CUR, r)?, ctx)?,
                            None => Value::Null,
                        }
                    }
                }
                Func::First => match &self.fixed[ci] {
                    Some(v) => v.clone(),
                    None => {
                        let v = eval(&c.args[0], self.at(3 + ci, 0)?, ctx)?;
                        self.fixed[ci] = Some(v.clone());
                        v
                    }
                },
                Func::Last => {
                    let row = self.at(3 + ci, e - 1)?;
                    eval(&c.args[0], row, ctx)?
                }
                Func::Nth(n) => {
                    if n > e {
                        Value::Null
                    } else {
                        match &self.fixed[ci] {
                            Some(v) => v.clone(),
                            None => {
                                let v = eval(&c.args[0], self.at(3 + ci, n - 1)?, ctx)?;
                                self.fixed[ci] = Some(v.clone());
                                v
                            }
                        }
                    }
                }
                Func::Agg => {
                    let WinFunc::Agg(agg) = &c.func else { unreachable!() };
                    match &self.results[ci] {
                        Some((fed, v)) if *fed == self.fed => v.clone(),
                        _ => {
                            if self.states[ci].is_none() {
                                self.states[ci] = Some(new_state(agg, eval_all(&agg.direct, &[], ctx)?)?);
                            }
                            let v = peek(agg, self.states[ci].as_mut().unwrap(), ctx)?;
                            self.results[ci] = Some((self.fed, v.clone()));
                            v
                        }
                    }
                }
            };
            out.push(v);
        }
        // The row goes up with them.
        let mut row = match &mut self.disk {
            None => self.rows[r - self.deleted].clone(),
            Some(_) => {
                self.at(CUR, r)?;
                self.disk.as_ref().unwrap().cursors[CUR].row.clone()
            }
        };
        row.reserve_exact(out.len());
        row.extend(out);
        // The read pointers' oldest row, and the trim after this row (`WinStore::partition`).
        let m_now = if self.ended { self.spooled } else { usize::MAX };
        let mut old = r + 1;
        if spec.ranged && spec.end == End::Peers {
            old = old.min(e + 1);
        }
        if spec.aggs {
            old = old.min((e + 1).min(m_now));
        }
        for (ci, f) in spec.funcs.iter().enumerate() {
            let mark = &mut self.marks[ci];
            match *f {
                Func::RowNumber | Func::Rank | Func::DenseRank | Func::PercentRank | Func::CumeDist => *mark = (*mark).max(r),
                Func::Ntile | Func::Agg => {}
                Func::Offset(d) => {
                    let t = (r as i64).saturating_add(d);
                    if t >= 0 && (t as usize) < m_now {
                        *mark = (*mark).max(t as usize);
                    }
                }
                // (first_value's mark stays at the partition's first row.)
                Func::First => {}
                Func::Last => *mark = (*mark).max(e - 1),
                Func::Nth(n) => {
                    if n - 1 < e {
                        *mark = (*mark).max(n - 1);
                    }
                }
            }
            if *f != Func::Agg {
                old = old.min(*mark);
            }
        }
        self.trim(old, r, mem);
        self.r += 1;
        Ok(Some(row))
    }

    /// tuplestore_trim after row `r` with the oldest read pointer at `oldest`: the rows
    /// before the one before it freed (none once on disk).
    fn trim(&mut self, oldest: usize, r: usize, mem: &mut StoreMem) {
        if self.disk.is_some() {
            return;
        }
        let oldest = oldest.min(r + 1).min(self.spooled);
        let nremove = oldest.saturating_sub(1);
        if nremove <= self.base {
            return;
        }
        let from = self.deleted.max(self.base);
        let to = nremove.max(self.deleted);
        let freed: Vec<i64> = (from..to).map(|p| self.tlens[p - self.deleted]).collect();
        let compact = if nremove - self.base >= (self.spooled - self.base) / 8 { nremove - self.base } else { 0 };
        mem.trim(&freed, compact);
        while self.deleted < to {
            self.rows.pop_front();
            self.tlens.pop_front();
            self.deleted += 1;
        }
        self.base += compact;
    }

    /// release_partition: the store cleared (its file closed and counted).
    pub(super) fn release(mut self, mem: &mut StoreMem) {
        mem.clear();
        if let Some(mut d) = self.disk.take()
            && let Some(a) = d.acct.take()
        {
            a.close_later(d.pg_bytes, &self.cleanup);
        }
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if let Some(mut d) = self.disk.take()
            && let Some(a) = d.acct.take()
        {
            a.close_later(d.pg_bytes, &self.cleanup);
        }
    }
}

fn cmp_keys(a: &[Value], b: &[Value], keys: &[SortKey], ctx: &ExecCtx) -> Result<Ordering> {
    for (i, k) in keys.iter().enumerate() {
        let o = super::nodes::compare_values(&a[i], &b[i], k, ctx)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}
