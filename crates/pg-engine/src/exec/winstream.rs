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
//! its temporary file has PostgreSQL's size. Frames are found as nodeWindowAgg.c finds them,
//! a head and a tail pointer moving forward (update_frameheadpos / update_frametailpos), and
//! each function computes over the frame what `window::compute_partition` computes: an
//! aggregate over a frame from the partition's start grows, one with a moving
//! implementation runs it (`window::Moving`), any other starts again each row.

use super::agg::{AggState, feed, finish, new_state, peek};
use super::spill::{RowReader, TempAcct, TempCleanup, TempFile, put_row};
use super::window::{Moving, MovingState, offset_value, range_stop};
use super::workmem::{BLCKSZ, MINIMAL_TUPLE_DATA_OFFSET, StoreMem, minimal_tuple_len_cols};
use super::{ExecCtx, Row, eval_all};
use crate::error::{Error, Result, code};
use crate::sql::expr::eval;
use crate::sql::plan::{FrameBound, FrameExclude, FrameUnit, SortKey, WinFunc, WinKind, WindowCall};
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::rc::Rc;

/// How an aggregate computes over its frame (as `compute_partition`).
#[derive(Clone, Copy, PartialEq)]
enum AggMode {
    /// From the partition's start, no exclusion: one state fed as the frame's end moves.
    Grow,
    /// The head moves: rows leaving it removed by the inverse transition (`window::Moving`).
    Moving,
    /// A new state over the frame's rows each row.
    Restart,
}

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
    Agg(AggMode),
}

/// A frame bound.
#[derive(Clone)]
enum Bound {
    /// UNBOUNDED PRECEDING (a head) or FOLLOWING (a tail).
    Unbounded,
    CurrentRow,
    /// ROWS / GROUPS n PRECEDING (negative) or FOLLOWING.
    Count(i64),
    /// RANGE offset PRECEDING (true) or FOLLOWING.
    Range(Value, bool),
}

/// The calls of one window, as this runs them.
pub(super) struct Spec {
    funcs: Vec<Func>,
    unit: FrameUnit,
    start: Bound,
    end: Bound,
    exclude: FrameExclude,
    ordered: bool,
    aggs: bool,
    /// Trailing length words on tape (the store was made EXEC_FLAG_BACKWARD).
    pub backward: bool,
    /// The current row's peer group's end / start is kept (a function, the frame or its
    /// exclusion reads it).
    peer_end: bool,
    peer_start: bool,
    /// Some function reads the partition's size: it is read whole.
    all: bool,
    /// count_store's read pointers: a frame head / tail pointer (RANGE / GROUPS), and the
    /// aggregates' mark at the frame's head.
    head_ptr: bool,
    tail_ptr: bool,
    agg_mark: bool,
}

impl Spec {
    /// The calls (one window) when this runs each of them: any frame (an exclusion of peers
    /// only with an ORDER BY), ranking functions, lag / lead / nth_value with constant
    /// offsets and no IGNORE NULLS, aggregates without DISTINCT or ORDER BY. None: the
    /// partition is computed whole.
    pub(super) fn of(calls: &[WindowCall], ctx: &ExecCtx) -> Option<Spec> {
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
        let (unit, start, end, exclude) = if all_frameless {
            (FrameUnit::Rows, FrameBound::UnboundedPreceding, FrameBound::CurrentRow, FrameExclude::NoOthers)
        } else {
            (f.unit, f.start.clone(), f.end.clone(), f.exclude)
        };
        let ordered = !c0.order_by.is_empty();
        let ranged = unit != FrameUnit::Rows;
        let excl_peers = matches!(exclude, FrameExclude::Group | FrameExclude::Ties);
        if excl_peers && !ordered {
            return None;
        }
        let bound = |b: &FrameBound, is_start: bool| -> Option<Bound> {
            Some(match b {
                FrameBound::UnboundedPreceding | FrameBound::UnboundedFollowing => Bound::Unbounded,
                FrameBound::CurrentRow => Bound::CurrentRow,
                FrameBound::Preceding(e) | FrameBound::Following(e) => {
                    let preceding = matches!(b, FrameBound::Preceding(_));
                    // (An offset that errors does so where the partition is computed whole.)
                    let v = offset_value(e, ctx, unit, is_start).ok()?;
                    if unit == FrameUnit::Range {
                        if c0.order_by.len() != 1 {
                            return None;
                        }
                        Bound::Range(v, preceding)
                    } else {
                        let k = v.as_i64().unwrap_or(0).max(0);
                        Bound::Count(if preceding { -k } else { k })
                    }
                }
            })
        };
        let (start_b, end_b) = (bound(&start, true)?, bound(&end, false)?);
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
                    Func::Agg(if Moving::of(c).is_some() {
                        AggMode::Moving
                    } else if matches!(start_b, Bound::Unbounded) && exclude == FrameExclude::NoOthers {
                        AggMode::Grow
                    } else {
                        AggMode::Restart
                    })
                }
            });
        }
        let aggs = funcs.iter().any(|f| matches!(f, Func::Agg(_)));
        let builtins = funcs.iter().any(|f| !matches!(f, Func::Agg(_)));
        let offset = |b: &Bound| matches!(b, Bound::Count(_) | Bound::Range(..));
        let current = |b: &Bound| matches!(b, Bound::CurrentRow);
        let head_ptr = ranged && (current(&start_b) && ordered || offset(&start_b));
        let tail_ptr = ranged && (current(&end_b) && ordered || offset(&end_b));
        let agg_mark = aggs && (!matches!(start_b, Bound::Unbounded) || exclude != FrameExclude::NoOthers);
        // (A RANGE / GROUPS frame ending at the current row has a tail pointer, kept at the
        // peer group's end whatever the functions read: ExecWindowAgg's update_frametailpos.)
        let peer_end = ordered && (funcs.contains(&Func::CumeDist) || ranged && current(&end_b) || excl_peers);
        let peer_start = ordered
            && (funcs.iter().any(|f| matches!(f, Func::Rank | Func::DenseRank | Func::PercentRank))
                || unit == FrameUnit::Groups
                || ranged && current(&start_b)
                || excl_peers);
        let all = funcs.iter().any(|f| matches!(f, Func::PercentRank | Func::CumeDist | Func::Ntile));
        Some(Spec {
            funcs,
            unit,
            start: start_b,
            end: end_b,
            exclude,
            ordered,
            aggs,
            backward: builtins || agg_mark,
            peer_end,
            peer_start,
            all,
            head_ptr,
            tail_ptr,
            agg_mark,
        })
    }

    fn ranged(&self) -> bool {
        self.unit != FrameUnit::Rows
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

/// The partition's rows on disk, in order: where each row from `offs_lo` on (those a
/// pointer may still read) begins, and a reader per cursor.
struct Disk {
    file: TempFile,
    acct: Option<TempAcct>,
    pg_bytes: u64,
    offs: VecDeque<u64>,
    offs_lo: usize,
    cursors: Vec<Cursor>,
    buf: Vec<u8>,
}

impl Disk {
    fn append(&mut self, row: &Row, tlen: i64, backward: bool) -> Result<()> {
        self.buf.clear();
        put_row(&mut self.buf, row);
        let (at, _) = self.file.append(&self.buf)?;
        self.offs.push_back(at);
        self.pg_bytes += tape_len(tlen, backward);
        Ok(())
    }
}

/// A read pointer into the file: the row it read last (at `pos - 1`), and where the next is.
#[derive(Default)]
struct Cursor {
    pos: usize,
    off: u64,
    rd: Option<RowReader>,
    row: Row,
}

// The cursors: the current row, the peer group's end, the aggregates' next row, the frame's
// head and tail pointers, a frame's rows read again (aggregates that start again, a moving
// aggregate's rows coming in), the rows a moving aggregate removes, then one per call (lag /
// lead's row, first / last / nth_value's).
const CUR: usize = 0;
const PEER: usize = 1;
const AGG: usize = 2;
const HEAD: usize = 3;
const TAIL: usize = 4;
const SCAN: usize = 5;
const REMOVE: usize = 6;
const CALLS: usize = 7;

/// A RANGE / GROUPS frame pointer: the row it is at, its peer group's number (GROUPS) and
/// its ORDER BY values.
#[derive(Default)]
struct Ptr {
    pos: usize,
    group: i64,
    keys: Option<Vec<Value>>,
}

/// A moving aggregate's state, as `window::moving_aggregate` keeps it: the rows from `base`
/// to `upto` aggregated, and the last result.
struct MovingRun {
    st: MovingState,
    base: usize,
    upto: usize,
    result: Value,
}

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
    /// The frame's head and tail pointers (RANGE / GROUPS offsets).
    head: Ptr,
    tail: Ptr,
    /// The aggregates' states (by call), the rows they have read, their results for that.
    states: Vec<Option<AggState>>,
    fed: usize,
    results: Vec<Option<(usize, Value)>>,
    moving: Vec<Option<MovingRun>>,
    /// first_value's and nth_value's last results, and the row they were read at (by call).
    fixed: Vec<Option<(usize, Value)>>,
    /// Each builtin's mark (by call).
    marks: Vec<usize>,
    cleanup: Rc<TempCleanup>,
}

/// What the window keeps of its input's rows for PostgreSQL's tuple sizes.
pub(super) struct Shape<'a> {
    pub types: &'a [DataType],
    pub keep: Option<&'a [bool]>,
}

/// Where the reading of rows goes: the source, PostgreSQL's count, the tuple shape.
struct Io<'s, 'a> {
    src: &'s mut dyn Source,
    mem: &'s mut StoreMem,
    shape: &'s Shape<'a>,
    backward: bool,
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
            head: Ptr::default(),
            tail: Ptr::default(),
            states: (0..n).map(|_| None).collect(),
            fed: 0,
            results: vec![None; n],
            moving: (0..n).map(|_| None).collect(),
            fixed: vec![None; n],
            marks: vec![0; n],
            cleanup: ctx.temp_cleanup.clone(),
        }
    }

    /// Read rows in until `want` are (or the partition ends).
    fn spool(&mut self, want: usize, io: &mut Io, ctx: &ExecCtx) -> Result<()> {
        while self.spooled < want && !self.ended {
            let Some(row) = io.src.pull(ctx)? else {
                self.ended = true;
                break;
            };
            let tlen = minimal_tuple_len_cols(&row, io.shape.types, io.shape.keep, ctx);
            if io.mem.put(tlen) {
                // (dumptuples: the rows held so far go to the file, then this one.)
                for &t in &self.tlens {
                    io.mem.dumped(t);
                }
                io.mem.dumped(tlen);
                self.dump(ctx, io.backward)?;
            }
            match &mut self.disk {
                Some(d) => {
                    d.append(&row, tlen, io.backward)?;
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

    /// Whether the row at `p` is in the partition (read in as far as it).
    fn has(&mut self, p: usize, io: &mut Io, ctx: &ExecCtx) -> Result<bool> {
        self.spool(p.saturating_add(1), io, ctx)?;
        Ok(p < self.spooled)
    }

    /// The rows in memory go to a new file; PostgreSQL's file holds them too.
    fn dump(&mut self, ctx: &ExecCtx, backward: bool) -> Result<()> {
        let mut d = Disk {
            file: TempFile::create_raw(ctx)?,
            acct: Some(TempAcct::new(ctx)),
            pg_bytes: 0,
            offs: VecDeque::new(),
            offs_lo: self.deleted,
            cursors: Vec::new(),
            buf: Vec::new(),
        };
        for (row, &t) in std::mem::take(&mut self.rows).iter().zip(&self.tlens) {
            d.append(row, t, backward)?;
        }
        if let Some(a) = &d.acct {
            a.pg_size(d.pg_bytes / BLCKSZ as u64 * BLCKSZ as u64)?;
        }
        self.tlens.clear();
        self.disk = Some(d);
        Ok(())
    }

    /// The row at position `p` (spooled, not freed), through cursor `cur` when on disk (a
    /// cursor reads on from where it is, or seeks back).
    fn at(&mut self, cur: usize, p: usize) -> Result<&Row> {
        debug_assert!(p >= self.deleted && p < self.spooled);
        let Some(d) = self.disk.as_mut() else {
            return Ok(&self.rows[p - self.deleted]);
        };
        while d.cursors.len() <= cur {
            d.cursors.push(Cursor { pos: d.offs_lo, off: d.offs.front().copied().unwrap_or(0), ..Default::default() });
        }
        let c = &mut d.cursors[cur];
        if p + 1 < c.pos || p >= c.pos + 64 {
            // (Back, or far ahead: from where row p begins.)
            debug_assert!(p >= d.offs_lo, "a row no pointer reaches");
            c.pos = p;
            c.off = d.offs[p - d.offs_lo];
            c.rd = None;
        }
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

    /// The ORDER BY values of the row at `p`.
    fn keys_at(&mut self, cur: usize, p: usize, keys: &[SortKey], ctx: &ExecCtx) -> Result<Vec<Value>> {
        let row = self.at(cur, p)?;
        keys.iter().map(|k| eval(&k.expr, row, ctx)).collect()
    }

    /// The current row's peer group's end: rows read in while they are its peers.
    fn find_peer_end(&mut self, keys: &[SortKey], io: &mut Io, ctx: &ExecCtx) -> Result<()> {
        loop {
            if !self.has(self.ge, io, ctx)? {
                return Ok(());
            }
            let k = self.keys_at(PEER, self.ge, keys, ctx)?;
            if cmp_keys(&k, &self.gkeys, keys, ctx)? != Ordering::Equal {
                self.next_keys = Some(k);
                return Ok(());
            }
            self.ge += 1;
        }
    }

    /// A GROUPS pointer (`which`: HEAD or TAIL) moved on to the first row of peer group
    /// `target` (update_frameheadpos): its position, the partition's size past the last.
    fn to_group(&mut self, which: usize, target: i64, keys: &[SortKey], io: &mut Io, ctx: &ExecCtx) -> Result<usize> {
        if target <= 0 {
            return Ok(0);
        }
        let mut ptr = std::mem::take(if which == HEAD { &mut self.head } else { &mut self.tail });
        if ptr.keys.is_none() && self.has(ptr.pos, io, ctx)? {
            ptr.keys = Some(self.keys_at(which, ptr.pos, keys, ctx)?);
        }
        while ptr.group < target {
            let p = ptr.pos + 1;
            if ptr.keys.is_none() || !self.has(p, io, ctx)? {
                ptr.pos = ptr.pos.max(self.spooled);
                ptr.keys = None;
                break;
            }
            let k = self.keys_at(which, p, keys, ctx)?;
            if cmp_keys(&k, ptr.keys.as_deref().unwrap_or_default(), keys, ctx)? != Ordering::Equal {
                ptr.group += 1;
            }
            ptr.keys = Some(k);
            ptr.pos = p;
        }
        let pos = ptr.pos;
        *(if which == HEAD { &mut self.head } else { &mut self.tail }) = ptr;
        Ok(pos)
    }

    /// A RANGE pointer moved on to where the bound of the current row (ORDER BY value
    /// `cur`) stops (`window::range_stop`).
    #[allow(clippy::too_many_arguments)]
    fn to_range(&mut self, which: usize, cur: &Value, off: &Value, preceding: bool, key: &SortKey, io: &mut Io, ctx: &ExecCtx) -> Result<usize> {
        let is_start = which == HEAD;
        let mut ptr = std::mem::take(if is_start { &mut self.head } else { &mut self.tail });
        while self.has(ptr.pos, io, ctx)? {
            let v = match &ptr.keys {
                Some(k) => k[0].clone(),
                None => eval(&key.expr, self.at(which, ptr.pos)?, ctx)?,
            };
            if range_stop(&v, cur, off, preceding, is_start, key, ctx)? {
                ptr.keys = Some(vec![v]);
                break;
            }
            ptr.keys = None;
            ptr.pos += 1;
        }
        let pos = ptr.pos;
        *(if is_start { &mut self.head } else { &mut self.tail }) = ptr;
        Ok(pos)
    }

    /// Whether the row at `p` is excluded from the current row's frame (`window::excluded`).
    fn excluded(&self, spec: &Spec, p: usize) -> bool {
        let r = self.r;
        match spec.exclude {
            FrameExclude::NoOthers => false,
            FrameExclude::CurrentRow => p == r,
            FrameExclude::Group => p >= self.gs && p < self.ge,
            FrameExclude::Ties => p != r && p >= self.gs && p < self.ge,
        }
    }

    /// The rows the exclusion takes out, as up to two runs in order.
    fn excluded_runs(&self, spec: &Spec) -> [(usize, usize); 2] {
        let r = self.r;
        match spec.exclude {
            FrameExclude::NoOthers => [(0, 0); 2],
            FrameExclude::CurrentRow => [(r, r + 1), (0, 0)],
            FrameExclude::Group => [(self.gs, self.ge), (0, 0)],
            FrameExclude::Ties => [(self.gs, r), (r + 1, self.ge)],
        }
    }

    /// The `k`th row (from 0) of frame [s, e) not excluded, from its head (or its tail):
    /// what `compute_partition` picks of the frame's rows left after the exclusion.
    fn kth(&self, spec: &Spec, s: usize, e: usize, k: usize, from_tail: bool) -> Option<usize> {
        let runs = self.excluded_runs(spec);
        if !from_tail {
            let mut p = s.checked_add(k)?;
            for &(a, b) in &runs {
                let a = a.max(s);
                if b > a && p >= a {
                    p += b - a;
                }
            }
            return (p < e).then_some(p);
        }
        // (The same from the tail: positions counted back from e - 1.)
        let len = e.checked_sub(s)?;
        let mut q = k;
        for &(a, b) in runs.iter().rev() {
            let (a, b) = (a.max(s), b.min(e));
            if b > a && q >= e - b {
                q += b - a;
            }
        }
        (q < len).then(|| e - 1 - q)
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
        let io = &mut Io { src, mem, shape, backward: spec.backward };
        let r = self.r;
        if !self.has(r, io, ctx)? {
            return Ok(None);
        }
        let keys = &calls[0].order_by;
        // The current row's peers.
        if spec.ordered && (spec.peer_end || spec.peer_start) && (r == 0 || r >= self.ge) {
            let k = match self.next_keys.take() {
                Some(k) if spec.peer_end => k,
                _ => self.keys_at(CUR, r, keys, ctx)?,
            };
            let new = r == 0 || spec.peer_end || cmp_keys(&k, &self.gkeys, keys, ctx)? != Ordering::Equal;
            if new {
                self.gs = r;
                self.dense += 1;
                self.gkeys = k;
            }
            self.ge = r + 1;
            if spec.peer_end {
                self.find_peer_end(keys, io, ctx)?;
            }
        }
        // (A function reading the partition's size reads it all.)
        if spec.all {
            self.spool(usize::MAX, io, ctx)?;
        }
        // The frame [s, e), as far as known: a bound past the partition's end is clamped to
        // it once read (usize::MAX: its end).
        let ranged = spec.ranged();
        let group = self.dense - 1;
        let s = match &spec.start {
            Bound::Unbounded => 0,
            Bound::CurrentRow if !ranged => r,
            Bound::CurrentRow if spec.ordered => self.gs,
            Bound::CurrentRow => 0,
            Bound::Count(d) if !ranged => (r as i64).saturating_add(*d).max(0) as usize,
            Bound::Count(d) => self.to_group(HEAD, group.saturating_add(*d), keys, io, ctx)?,
            Bound::Range(off, preceding) => {
                let cur = eval(&keys[0].expr, self.at(CUR, r)?, ctx)?;
                self.to_range(HEAD, &cur, off, *preceding, &keys[0], io, ctx)?
            }
        };
        let e = match &spec.end {
            Bound::Unbounded => usize::MAX,
            Bound::CurrentRow if !ranged => r + 1,
            Bound::CurrentRow if spec.ordered => self.ge,
            Bound::CurrentRow => usize::MAX,
            Bound::Count(d) if !ranged => (r as i64).saturating_add(*d).saturating_add(1).max(0) as usize,
            Bound::Count(d) => {
                let t = group.saturating_add(*d);
                if t < 0 { 0 } else { self.to_group(TAIL, t.saturating_add(1), keys, io, ctx)? }
            }
            Bound::Range(off, preceding) => {
                let cur = eval(&keys[0].expr, self.at(CUR, r)?, ctx)?;
                self.to_range(TAIL, &cur, off, *preceding, &keys[0], io, ctx)?
            }
        };
        // What the pointers have read in (count_store's spool; update_frametailpos's).
        let tail_spool = match &spec.end {
            Bound::Unbounded => usize::MAX,
            Bound::CurrentRow if !ranged => r + 1,
            Bound::CurrentRow if !spec.ordered => usize::MAX,
            Bound::Count(_) if !ranged => e,
            _ => e.saturating_add(1),
        };
        let mut sp = r + 1;
        if spec.tail_ptr {
            sp = sp.max(tail_spool);
        }
        if spec.aggs {
            sp = sp.max(e.saturating_add(1));
        }
        for f in &spec.funcs {
            match *f {
                Func::Offset(d) => sp = sp.max((r as i64).saturating_add(d).saturating_add(1).max(0) as usize),
                Func::Nth(n) => sp = sp.max(s.saturating_add(n)),
                Func::Last => sp = sp.max(tail_spool),
                Func::First => sp = sp.max(s.saturating_add(1)),
                _ => {}
            }
        }
        self.spool(sp, io, ctx)?;
        let m = self.ended.then_some(self.spooled);
        let (s, e) = match m {
            Some(m) => (s.min(m), e.min(m)),
            None => (s, e),
        };
        let e = e.max(s);
        // The results.
        let mut out: Vec<Value> = Vec::with_capacity(spec.funcs.len());
        if spec.funcs.contains(&Func::Agg(AggMode::Grow)) && self.fed < e {
            while self.fed < e {
                let p = self.fed;
                self.at(AGG, p)?;
                let row = match &self.disk {
                    Some(d) => &d.cursors[AGG].row,
                    None => &self.rows[p - self.deleted],
                };
                for ((f, c), st) in spec.funcs.iter().zip(calls).zip(self.states.iter_mut()) {
                    if *f != Func::Agg(AggMode::Grow) {
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
        // (Aggregates that start again: one pass over the frame's rows for all of them.)
        let mut restarted: Vec<Option<AggState>> = Vec::new();
        if spec.funcs.contains(&Func::Agg(AggMode::Restart)) {
            for (f, c) in spec.funcs.iter().zip(calls) {
                restarted.push(match (f, &c.func) {
                    (Func::Agg(AggMode::Restart), WinFunc::Agg(agg)) => Some(new_state(agg, eval_all(&agg.direct, &[], ctx)?)?),
                    _ => None,
                });
            }
            for p in s..e {
                if self.excluded(spec, p) {
                    continue;
                }
                let row = self.at(SCAN, p)?;
                for (st, c) in restarted.iter_mut().zip(calls) {
                    if let (Some(st), WinFunc::Agg(agg)) = (st.as_mut(), &c.func) {
                        feed(agg, st, row, ctx)?;
                    }
                }
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
                        let row = self.at(CALLS + ci, t as usize)?;
                        eval(&c.args[0], row, ctx)?
                    } else {
                        match c.args.get(2) {
                            Some(dflt) => eval(dflt, self.at(CUR, r)?, ctx)?,
                            None => Value::Null,
                        }
                    }
                }
                Func::First | Func::Last | Func::Nth(_) => {
                    let at = match *f {
                        Func::First => self.kth(spec, s, e, 0, false),
                        Func::Last => self.kth(spec, s, e, 0, true),
                        Func::Nth(n) => self.kth(spec, s, e, n - 1, false),
                        _ => unreachable!(),
                    };
                    match at {
                        Some(p) if self.has(p, io, ctx)? => match &self.fixed[ci] {
                            Some((q, v)) if *q == p => v.clone(),
                            _ => {
                                let v = eval(&c.args[0], self.at(CALLS + ci, p)?, ctx)?;
                                self.fixed[ci] = Some((p, v.clone()));
                                v
                            }
                        },
                        _ => Value::Null,
                    }
                }
                Func::Agg(AggMode::Grow) => {
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
                Func::Agg(AggMode::Restart) => {
                    let WinFunc::Agg(agg) = &c.func else { unreachable!() };
                    finish(agg, restarted[ci].take().expect("a restarted aggregate"), ctx)?
                }
                Func::Agg(AggMode::Moving) => self.moving_next(spec, c, ci, s, e, ctx)?,
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
        // The read pointers' oldest row, and the trim after this row (`count_store`,
        // `WinStore::partition`).
        let m_now = m.unwrap_or(usize::MAX);
        let excl = spec.exclude != FrameExclude::NoOthers;
        let mut old = r + 1;
        if spec.tail_ptr {
            old = old.min(e.saturating_add(1));
        }
        if spec.head_ptr {
            old = old.min(s + 1);
        }
        if spec.aggs {
            old = old.min(e.saturating_add(1).min(m_now));
            if spec.agg_mark {
                old = old.min(s);
            }
        }
        for (ci, f) in spec.funcs.iter().enumerate() {
            let mark = &mut self.marks[ci];
            match *f {
                Func::RowNumber | Func::Rank | Func::DenseRank | Func::PercentRank | Func::CumeDist => *mark = (*mark).max(r),
                Func::Ntile | Func::Agg(_) => {}
                Func::Offset(d) => {
                    let t = (r as i64).saturating_add(d);
                    if t >= 0 && (t as usize) < m_now {
                        *mark = (*mark).max(t as usize);
                    }
                }
                Func::First => {
                    if s < e {
                        *mark = (*mark).max(s);
                    }
                }
                Func::Last => {
                    if s < e {
                        *mark = (*mark).max(if excl { s } else { e - 1 });
                    }
                }
                Func::Nth(n) => {
                    let a = s.saturating_add(n - 1);
                    if a < e {
                        *mark = (*mark).max(if excl { s } else { a });
                    }
                }
            }
            if !matches!(f, Func::Agg(_)) {
                old = old.min(*mark);
            }
        }
        self.trim(old, r, io.mem);
        self.r += 1;
        Ok(Some(row))
    }

    /// A moving aggregate's result for the current row's frame [head, tail): `moving_aggregate`'s
    /// step for one row.
    fn moving_next(&mut self, spec: &Spec, call: &WindowCall, ci: usize, head: usize, tail: usize, ctx: &ExecCtx) -> Result<Value> {
        let mova = Moving::of(call).expect("a moving aggregate");
        let i = self.r;
        let exclusion = spec.exclude != FrameExclude::NoOthers;
        let end_reuses = matches!(spec.end, Bound::Unbounded | Bound::CurrentRow);
        let mut run = match self.moving[ci].take() {
            Some(run) => run,
            None => MovingRun { st: mova.init(ctx)?, base: 0, upto: 0, result: Value::Null },
        };
        // The frame is the previous row's: its result stands.
        if i > 0 && run.base == head && end_reuses && !exclusion && run.base <= i && run.upto > i {
            let v = run.result.clone();
            self.moving[ci] = Some(run);
            return Ok(v);
        }
        let mut restart = i == 0 || exclusion || run.upto <= head;
        while !restart && run.base < head {
            let row = self.at(REMOVE, run.base)?;
            if !mova.remove(&mut run.st, row, ctx)? {
                restart = true;
            }
            run.base += 1;
        }
        run.base = head;
        if restart {
            run.st = mova.init(ctx)?;
            run.upto = head;
        }
        while run.upto < tail {
            if !self.excluded(spec, run.upto) {
                let row = self.at(SCAN, run.upto)?;
                mova.advance(&mut run.st, row, ctx)?;
            }
            run.upto += 1;
        }
        run.result = mova.finalize(&run.st, ctx)?;
        let v = run.result.clone();
        self.moving[ci] = Some(run);
        Ok(v)
    }

    /// tuplestore_trim after row `r` with the oldest read pointer at `oldest`: the rows
    /// before the one before it freed (none once on disk: only where they begin is dropped).
    fn trim(&mut self, oldest: usize, r: usize, mem: &mut StoreMem) {
        let oldest = oldest.min(r + 1).min(self.spooled);
        let nremove = oldest.saturating_sub(1);
        if let Some(d) = self.disk.as_mut() {
            while d.offs_lo < nremove && !d.offs.is_empty() {
                d.offs.pop_front();
                d.offs_lo += 1;
            }
            return;
        }
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
