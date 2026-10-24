//! Window functions: partitions, ordering, frames (ROWS / RANGE / GROUPS with offsets and
//! exclusions), ranking and value functions, and aggregates over frames.

use super::agg::{feed, finish, new_state};
use super::nodes::compare_values;
use super::{BoxNode, ExecCtx, Node, Row, eval_all};
use crate::error::{Error, Result, code};
use crate::sql::expr::eval;
use crate::sql::plan::{
    FrameBound, FrameExclude, FrameUnit, SortKey, WinFunc, WinKind, WindowCall,
};
use crate::types::numeric::Numeric;
use crate::types::value::Interval;
use crate::types::{DataType, Value};
use std::cmp::Ordering;

pub struct WindowNode {
    input: Option<BoxNode>,
    calls: Vec<WindowCall>,
    out: std::vec::IntoIter<Row>,
    /// The sort keys the input is already in (a Sort below), as `key_repr`s.
    presorted: Vec<String>,
}

impl WindowNode {
    pub fn new(input: BoxNode, calls: Vec<WindowCall>) -> Self {
        WindowNode {
            input: Some(input),
            calls,
            out: Vec::new().into_iter(),
            presorted: Vec::new(),
        }
    }

    /// The input is sorted on `keys` (a Sort right below).
    pub fn presorted(mut self, keys: &[SortKey]) -> Self {
        self.presorted = keys
            .iter()
            .map(|k| key_repr(&k.expr, k.desc, k.nulls_first))
            .collect();
        self
    }
}

impl Node for WindowNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if let Some(mut input) = self.input.take() {
            let mut rows = Vec::new();
            while let Some(r) = input.next(ctx)? {
                ctx.check()?;
                rows.push(r);
            }
            let n = rows.len();
            let mut results: Vec<Vec<Value>> = vec![vec![Value::Null; self.calls.len()]; n];
            let order = run_windows(&self.calls, &rows, &mut results, &self.presorted, ctx)?;
            let mut out = Vec::with_capacity(n);
            let mut rows: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
            for i in order {
                let mut r = rows[i].take().unwrap();
                r.extend(std::mem::take(&mut results[i]));
                out.push(r);
            }
            self.out = out.into_iter();
        }
        Ok(self.out.next())
    }
}

/// A sort key as the window planning compares them.
fn key_repr(e: &crate::sql::expr::Expr, desc: bool, nulls_first: bool) -> String {
    format!("{e:?}/{desc}/{nulls_first}")
}

/// A key a window's sort needs: a partition key (compared under its collation when that
/// doesn't compare bytes) or an ORDER BY key.
struct NeedKey {
    key: SortKey,
    coll: Option<std::sync::Arc<crate::types::collation::Collation>>,
    repr: String,
}

fn cmp_need(a: &Value, b: &Value, k: &NeedKey, ctx: &ExecCtx) -> Result<Ordering> {
    match (&k.coll, a.is_null() || b.is_null()) {
        (Some(c), false) => Ok(crate::types::collation::cmp_values(c, a, b)),
        _ => compare_values(a, b, &k.key, ctx),
    }
}

/// The window calls run as Postgres's WindowAgg nodes do: calls with the same window
/// together, the windows in select_active_windows's order, each over the input sorted
/// for it (a Sort, or an Incremental Sort when a prefix of its keys is sorted already)
/// and split into partitions as the sorted rows go by. Returns the rows' final order.
fn run_windows(
    calls: &[WindowCall],
    rows: &[Row],
    results: &mut [Vec<Value>],
    presorted: &[String],
    ctx: &ExecCtx,
) -> Result<Vec<usize>> {
    use crate::sql::binder::optimize::pseudo_constant;
    let n = rows.len();
    // Windows: calls sharing partitioning, ordering and frame.
    let mut wins: Vec<(String, Vec<usize>)> = Vec::new();
    for (ci, call) in calls.iter().enumerate() {
        let part: Vec<String> = call
            .partition_by
            .iter()
            .filter(|e| !pseudo_constant(e))
            .map(|e| key_repr(e, false, false))
            .collect();
        let order: Vec<String> = call
            .order_by
            .iter()
            .map(|k| key_repr(&k.expr, k.desc, k.nulls_first))
            .collect();
        let spec = format!("{part:?}{order:?}{:?}", call.frame);
        match wins.iter_mut().find(|w| w.0 == spec) {
            Some(w) => w.1.push(ci),
            None => wins.push((spec, vec![ci])),
        }
    }
    // select_active_windows (as EXPLAIN orders them).
    let mut refs: Vec<String> = Vec::new();
    let mut ref_of = |e: &crate::sql::expr::Expr| {
        let k = format!("{e:?}");
        match refs.iter().position(|r| *r == k) {
            Some(i) => i,
            None => {
                refs.push(k);
                refs.len() - 1
            }
        }
    };
    let mut unique_orders: Vec<Vec<(usize, bool, bool)>> = Vec::new();
    for (_, cs) in &wins {
        let call = &calls[cs[0]];
        for k in &call.order_by {
            ref_of(&k.expr);
        }
        for e in &call.partition_by {
            ref_of(e);
        }
        let mut u: Vec<(usize, bool, bool)> = Vec::new();
        for e in &call.partition_by {
            let x = (ref_of(e), false, false);
            if !u.contains(&x) {
                u.push(x);
            }
        }
        for k in &call.order_by {
            let x = (ref_of(&k.expr), k.desc, k.nulls_first);
            if !u.contains(&x) {
                u.push(x);
            }
        }
        unique_orders.push(u);
    }
    let mut sequence: Vec<usize> = (0..wins.len()).collect();
    sequence.sort_by(|&a, &b| {
        for (x, y) in unique_orders[a].iter().zip(&unique_orders[b]) {
            let o = y
                .0
                .cmp(&x.0)
                .then(x.1.cmp(&y.1).reverse())
                .then(x.2.cmp(&y.2).reverse());
            if o != Ordering::Equal {
                return o;
            }
        }
        unique_orders[b].len().cmp(&unique_orders[a].len())
    });
    let mut order: Vec<usize> = (0..n).collect();
    let mut sorted: Vec<String> = presorted.to_vec();
    // (No window's results yet: rows that are the same are the same in every way.)
    let mut first = true;
    for wi in sequence {
        let call0 = &calls[wins[wi].1[0]];
        let part_exprs: Vec<&crate::sql::expr::Expr> = call0
            .partition_by
            .iter()
            .filter(|e| !pseudo_constant(e))
            .collect();
        let colls = partition_collations(
            &part_exprs.iter().map(|e| (*e).clone()).collect::<Vec<_>>(),
            ctx,
        );
        let mut need: Vec<NeedKey> = Vec::new();
        for (e, coll) in part_exprs.iter().zip(colls) {
            let repr = key_repr(e, false, false);
            if !need.iter().any(|k| k.repr == repr) {
                need.push(NeedKey {
                    key: SortKey {
                        expr: (*e).clone(),
                        desc: false,
                        nulls_first: false,
                        using: None,
                    },
                    coll,
                    repr,
                });
            }
        }
        for k in call0.order_by.iter().filter(|k| !pseudo_constant(&k.expr)) {
            let repr = key_repr(&k.expr, k.desc, k.nulls_first);
            if !need.iter().any(|x| x.repr == repr) {
                need.push(NeedKey {
                    key: k.clone(),
                    coll: None,
                    repr,
                });
            }
        }
        let common = need
            .iter()
            .zip(&sorted)
            .take_while(|(a, b)| a.repr == **b)
            .count();
        if !need.is_empty() && common < need.len() {
            let vals: Vec<Vec<Value>> = rows
                .iter()
                .map(|r| need.iter().map(|k| eval(&k.key.expr, r, ctx)).collect())
                .collect::<Result<_>>()?;
            let cmp_range = |a: usize, b: usize, from: usize, to: usize| -> Result<Ordering> {
                for (i, k) in need.iter().enumerate().take(to).skip(from) {
                    match cmp_need(&vals[a][i], &vals[b][i], k, ctx)? {
                        Ordering::Equal => {}
                        o => return Ok(o),
                    }
                }
                Ok(Ordering::Equal)
            };
            let len = need.len();
            let mut full = |a: &usize, b: &usize| cmp_range(*a, *b, 0, len);
            let mut cf = super::pgsort::Cmp::new(&mut full);
            if common > 0 {
                let mut suffix = |a: &usize, b: &usize| cmp_range(*a, *b, common, len);
                let mut cs = super::pgsort::Cmp::new(&mut suffix);
                let mut err = None;
                let mut same = |a: &usize, b: &usize| match cmp_range(*a, *b, 0, common) {
                    Ok(o) => o == Ordering::Equal,
                    Err(e) => {
                        err.get_or_insert(e);
                        true
                    }
                };
                let mut stats = super::pgsort::IncrStats::default();
                order = super::pgsort::incremental_sort(order, None, &mut cf, &mut cs, &mut same, &mut stats);
                if let Some(e) = err.or(cs.err) {
                    return Err(e);
                }
            } else if let Some(p) = first
                .then(|| {
                    let keys: Vec<SortKey> = need.iter().map(|k| k.key.clone()).collect();
                    // (Partition keys under a nondeterministic collation compare by it.)
                    if need.iter().any(|k| k.coll.is_some()) {
                        return None;
                    }
                    super::fastsort::order(n, &keys, |i, k| &vals[order[i]][k], |i| &rows[order[i]], None)
                })
                .flatten()
            {
                order = p.into_iter().map(|j| order[j]).collect();
            } else {
                order = super::pgsort::tuplesort(order, None, &mut cf);
            }
            if let Some(e) = cf.err {
                return Err(e);
            }
            sorted = need.iter().map(|k| k.repr.clone()).collect();
        }
        // Partitions: runs of rows whose partition keys are equal.
        let npart = need
            .iter()
            .take_while(|k| part_exprs.iter().any(|e| key_repr(e, false, false) == k.repr))
            .count();
        let pvals: Vec<Vec<Value>> = order
            .iter()
            .map(|&i| part_exprs.iter().map(|e| eval(e, &rows[i], ctx)).collect())
            .collect::<Result<_>>()?;
        let pkeys: Vec<&NeedKey> = need.iter().take(npart).collect();
        // (Each partition key's comparison, found once rather than per row.)
        let pk: Vec<Option<&NeedKey>> = part_exprs
            .iter()
            .map(|e| {
                let r = key_repr(e, false, false);
                pkeys.iter().find(|k| k.repr == r).copied()
            })
            .collect();
        let same_part = |a: &[Value], b: &[Value]| -> Result<bool> {
            for (i, k) in pk.iter().enumerate() {
                let o = match k {
                    Some(k) => cmp_need(&a[i], &b[i], k, ctx)?,
                    None => a[i].cmp_total(&b[i]),
                };
                if o != Ordering::Equal {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let mut parts: Vec<Vec<usize>> = Vec::new();
        for (p, &i) in order.iter().enumerate() {
            if p == 0 || !same_part(&pvals[p - 1], &pvals[p])? {
                parts.push(Vec::new());
            }
            parts.last_mut().unwrap().push(i);
        }
        for &ci in &wins[wi].1 {
            let call = &calls[ci];
            let okeys: Vec<Vec<Value>> = rows
                .iter()
                .map(|r| call.order_by.iter().map(|k| eval(&k.expr, r, ctx)).collect())
                .collect::<Result<_>>()?;
            let mut out = vec![Value::Null; n];
            for part in &parts {
                compute_partition(call, rows, &okeys, part, &mut out, ctx)?;
            }
            for (i, v) in out.into_iter().enumerate() {
                results[i][ci] = v;
            }
        }
        first = false;
    }
    Ok(order)
}

fn cmp_keys(a: &[Value], b: &[Value], keys: &[SortKey], ctx: &ExecCtx) -> Result<Ordering> {
    for (i, k) in keys.iter().enumerate() {
        let o = compare_values(&a[i], &b[i], k, ctx)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}


/// Peer group boundaries: for each position, [start, end) of its peers.
fn peer_groups(
    part: &[usize],
    okeys: &[Vec<Value>],
    keys: &[SortKey],
    ctx: &ExecCtx,
) -> Result<Vec<(usize, usize)>> {
    let m = part.len();
    let mut out = vec![(0, 0); m];
    let mut i = 0;
    while i < m {
        let mut j = i + 1;
        while j < m && cmp_keys(&okeys[part[i]], &okeys[part[j]], keys, ctx)? == Ordering::Equal {
            j += 1;
        }
        for item in out.iter_mut().take(j).skip(i) {
            *item = (i, j);
        }
        i = j;
    }
    Ok(out)
}

fn offset_value(
    e: &crate::sql::expr::Expr,
    ctx: &ExecCtx,
    unit: FrameUnit,
    start: bool,
) -> Result<Value> {
    let which = if start { "starting" } else { "ending" };
    let v = eval(e, &[], ctx)?;
    if v.is_null() {
        return Err(Error::new(
            code::NULL_VALUE_NOT_ALLOWED,
            format!("frame {which} offset must not be null"),
        ));
    }
    // RANGE offsets are checked by the in_range functions, when used.
    if unit != FrameUnit::Range && v.as_i64().is_some_and(|k| k < 0) {
        return Err(Error::new(
            code::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
            format!("frame {which} offset must not be negative"),
        ));
    }
    Ok(v)
}

fn invalid_size() -> Error {
    Error::new(
        code::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
        "invalid preceding or following size in window function",
    )
}

/// The btree in_range support functions: whether `val` is <= (`less`) or >= `base` ∓ `off`
/// (`sub`: minus), with their treatment of NaN and infinities.
fn in_range(
    val: &Value,
    base: &Value,
    off: &Value,
    sub: bool,
    less: bool,
    ctx: &ExecCtx,
) -> Result<bool> {
    use crate::types::datetime as dt;
    let pick = |o: Ordering| if less { o != Ordering::Greater } else { o != Ordering::Less };
    let timestamps = |val: i64, base: i64, iv: &Interval, tz: bool| -> Result<bool> {
        if iv.cmp_key() < 0 {
            return Err(invalid_size());
        }
        if iv.is_pos_inf() && (if sub { base == dt::TS_POS_INF } else { base == dt::TS_NEG_INF })
        {
            return Ok(true);
        }
        let iv = if sub { dt::interval_neg(iv)? } else { *iv };
        let sum = if tz {
            dt::tstz_add_interval(base, &iv, &ctx.fmt().tz)?
        } else {
            dt::ts_add_interval(base, &iv)?
        };
        Ok(pick(val.cmp(&sum)))
    };
    Ok(match (val, base, off) {
        (Value::Float4(_) | Value::Float8(_), _, _) => {
            let (v, b, o) = (
                val.as_f64().unwrap_or(0.0),
                base.as_f64().unwrap_or(0.0),
                off.as_f64().unwrap_or(0.0),
            );
            if o.is_nan() || o < 0.0 {
                return Err(invalid_size());
            }
            if v.is_nan() {
                return Ok(b.is_nan() || !less);
            }
            if b.is_nan() {
                return Ok(less);
            }
            if o.is_infinite() && b.is_infinite() && (if sub { b > 0.0 } else { b < 0.0 }) {
                return Ok(true);
            }
            let sum = if sub { b - o } else { b + o };
            if less { v <= sum } else { v >= sum }
        }
        (Value::Numeric(v), Value::Numeric(b), _) => {
            let o = match off {
                Value::Numeric(o) => (**o).clone(),
                o => Numeric::from_i64(o.as_i64().unwrap_or(0)),
            };
            if o.is_nan() || o.is_negative() {
                return Err(invalid_size());
            }
            if v.is_nan() {
                return Ok(b.is_nan() || !less);
            }
            if b.is_nan() {
                return Ok(less);
            }
            if matches!(o, Numeric::PosInf)
                && (if sub { matches!(**b, Numeric::PosInf) } else { matches!(**b, Numeric::NegInf) })
            {
                return Ok(true);
            }
            let sum = if sub { b.sub(&o) } else { b.add(&o) };
            pick((**v).cmp(&sum))
        }
        (Value::Interval(v), Value::Interval(b), Value::Interval(o)) => {
            if o.cmp_key() < 0 {
                return Err(invalid_size());
            }
            if o.is_pos_inf() && (if sub { b.is_pos_inf() } else { b.is_neg_inf() }) {
                return Ok(true);
            }
            let sum = if sub { dt::interval_sub(b, o)? } else { dt::interval_add(b, o)? };
            pick(v.cmp(&sum))
        }
        (Value::Timestamp(v), Value::Timestamp(b), Value::Interval(o)) => {
            timestamps(*v, *b, o, false)?
        }
        (Value::Timestamptz(v), Value::Timestamptz(b), Value::Interval(o)) => {
            timestamps(*v, *b, o, true)?
        }
        (Value::Date(v), Value::Date(b), Value::Interval(o)) => {
            let ts = |d: i32| match d {
                dt::DATE_NEG_INF => dt::TS_NEG_INF,
                dt::DATE_POS_INF => dt::TS_POS_INF,
                d => d as i64 * crate::types::value::USECS_PER_DAY,
            };
            timestamps(ts(*v), ts(*b), o, false)?
        }
        (Value::Time(v), Value::Time(b), Value::Interval(o)) => {
            // Only the time part counts (no wraparound); +infinity overflows.
            if o.micros < 0 {
                return Err(invalid_size());
            }
            let sum = if sub {
                b - o.micros
            } else {
                match b.checked_add(o.micros) {
                    Some(x) => x,
                    None => return Ok(less),
                }
            };
            pick(v.cmp(&sum))
        }
        (Value::Timetz(v, vz), Value::Timetz(b, bz), Value::Interval(o)) => {
            if o.micros < 0 {
                return Err(invalid_size());
            }
            let sum = if sub {
                b - o.micros
            } else {
                match b.checked_add(o.micros) {
                    Some(x) => x,
                    None => return Ok(less),
                }
            };
            pick(Value::Timetz(*v, *vz).cmp_total(&Value::Timetz(sum, *bz)))
        }
        _ => {
            // Integers (the offset as int8; no overflow in i128).
            let (v, b, o) = (
                val.as_i64().unwrap_or(0) as i128,
                base.as_i64().unwrap_or(0) as i128,
                off.as_i64().unwrap_or(0) as i128,
            );
            if o < 0 {
                return Err(invalid_size());
            }
            let sum = if sub { b - o } else { b + o };
            if less { v <= sum } else { v >= sum }
        }
    })
}

/// Frame [start, end) for position `i`, plus excluded positions.
#[allow(clippy::too_many_arguments)]
fn frame_of(
    call: &WindowCall,
    i: usize,
    m: usize,
    peers: &[(usize, usize)],
    part: &[usize],
    okeys: &[Vec<Value>],
    offsets: &(Option<Value>, Option<Value>),
    ctx: &ExecCtx,
) -> Result<(usize, usize)> {
    let f = &call.frame;
    let bound = |b: &FrameBound, off: &Option<Value>, is_start: bool| -> Result<usize> {
        Ok(match b {
            FrameBound::UnboundedPreceding => 0,
            FrameBound::UnboundedFollowing => m,
            FrameBound::CurrentRow => match f.unit {
                FrameUnit::Rows => {
                    if is_start {
                        i
                    } else {
                        i + 1
                    }
                }
                _ => {
                    if is_start {
                        peers[i].0
                    } else {
                        peers[i].1
                    }
                }
            },
            FrameBound::Preceding(_) | FrameBound::Following(_) => {
                let off = off.as_ref().unwrap();
                let preceding = matches!(b, FrameBound::Preceding(_));
                match f.unit {
                    FrameUnit::Rows => {
                        let k = off.as_i64().unwrap_or(0).max(0) as usize;
                        let pos = if preceding {
                            i.saturating_sub(k)
                        } else {
                            (i + k).min(m)
                        };
                        let pos = if preceding && k > i { 0 } else { pos };
                        if is_start {
                            pos.min(m)
                        } else {
                            (if preceding && k > i { 0 } else { pos + 1 }).min(m)
                        }
                    }
                    FrameUnit::Groups => {
                        // Count peer groups.
                        let k = off.as_i64().unwrap_or(0).max(0) as usize;
                        let mut starts = Vec::new();
                        let mut p = 0;
                        while p < m {
                            starts.push(p);
                            p = peers[p].1;
                        }
                        let g = starts.iter().position(|s| *s == peers[i].0).unwrap_or(0);
                        let target = if preceding {
                            g as i128 - k as i128
                        } else {
                            g as i128 + k as i128
                        };
                        if is_start {
                            if target < 0 {
                                0
                            } else if target >= starts.len() as i128 {
                                m
                            } else {
                                starts[target as usize]
                            }
                        } else if target < 0 {
                            0
                        } else if target >= starts.len() as i128 {
                            m
                        } else {
                            peers[starts[target as usize]].1
                        }
                    }
                    FrameUnit::Range => {
                        // nodeWindowAgg: the head is the first row that satisfies in_range
                        // (>= current ± offset), the tail the first after it that doesn't
                        // (<=); NULLs place by NULLS FIRST / LAST alone.
                        let key = call.order_by.first();
                        let (desc, nulls_first) =
                            key.map_or((false, false), |k| (k.desc, k.nulls_first));
                        let cur = &okeys[part[i]][0];
                        let (mut sub, mut less) = (preceding, !is_start);
                        if desc {
                            sub = !sub;
                            less = !less;
                        }
                        let mut p = 0;
                        while p < m {
                            let v = &okeys[part[p]][0];
                            let stop = if v.is_null() || cur.is_null() {
                                if is_start {
                                    if nulls_first {
                                        !v.is_null() || cur.is_null()
                                    } else {
                                        v.is_null() || !cur.is_null()
                                    }
                                } else if nulls_first {
                                    !v.is_null()
                                } else {
                                    !cur.is_null()
                                }
                            } else {
                                in_range(v, cur, off, sub, less, ctx)? == is_start
                            };
                            if stop {
                                break;
                            }
                            p += 1;
                        }
                        p
                    }
                }
            }
        })
    };
    let s = bound(&f.start, &offsets.0, true)?;
    let e = bound(&f.end, &offsets.1, false)?;
    Ok((s, e.max(s)))
}

fn excluded(call: &WindowCall, i: usize, p: usize, peers: &[(usize, usize)]) -> bool {
    match call.frame.exclude {
        FrameExclude::NoOthers => false,
        FrameExclude::CurrentRow => p == i,
        FrameExclude::Group => p >= peers[i].0 && p < peers[i].1,
        FrameExclude::Ties => p != i && p >= peers[i].0 && p < peers[i].1,
    }
}

fn compute_partition(
    call: &WindowCall,
    rows: &[Row],
    okeys: &[Vec<Value>],
    part: &[usize],
    out: &mut [Value],
    ctx: &ExecCtx,
) -> Result<()> {
    let m = part.len();
    let peers = peer_groups(part, okeys, &call.order_by, ctx)?;
    let offsets = (
        match &call.frame.start {
            FrameBound::Preceding(e) | FrameBound::Following(e) => {
                Some(offset_value(e, ctx, call.frame.unit, true)?)
            }
            _ => None,
        },
        match &call.frame.end {
            FrameBound::Preceding(e) | FrameBound::Following(e) => {
                Some(offset_value(e, ctx, call.frame.unit, false)?)
            }
            _ => None,
        },
    );
    match &call.func {
        WinFunc::Builtin(kind) => {
            let mut dense = 0i64;
            let mut last_group = usize::MAX;
            for i in 0..m {
                let r = &rows[part[i]];
                if peers[i].0 != last_group {
                    dense += 1;
                    last_group = peers[i].0;
                }
                let v = match kind {
                    WinKind::RowNumber => Value::Int8(i as i64 + 1),
                    WinKind::Rank => Value::Int8(peers[i].0 as i64 + 1),
                    WinKind::DenseRank => Value::Int8(dense),
                    WinKind::PercentRank => Value::Float8(if m <= 1 {
                        0.0
                    } else {
                        peers[i].0 as f64 / (m - 1) as f64
                    }),
                    WinKind::CumeDist => Value::Float8(peers[i].1 as f64 / m as f64),
                    WinKind::Ntile => {
                        let k = eval(&call.args[0], r, ctx)?;
                        if k.is_null() {
                            Value::Null
                        } else {
                            let k = k.as_i64().unwrap_or(0);
                            if k <= 0 {
                                return Err(Error::new(
                                    code::INVALID_ARGUMENT_FOR_NTILE,
                                    "argument of ntile must be greater than zero",
                                ));
                            }
                            let k = k as usize;
                            let base = m / k;
                            let extra = m % k;
                            // The first `extra` buckets have base+1 rows.
                            let bucket = if i < extra * (base + 1) {
                                i / (base + 1)
                            } else {
                                extra + (i - extra * (base + 1)) / base.max(1)
                            };
                            Value::Int8(bucket as i64 + 1)
                        }
                    }
                    WinKind::Lag | WinKind::Lead => {
                        let off = match call.args.get(1) {
                            Some(e) => match eval(e, r, ctx)? {
                                Value::Null => {
                                    out[part[i]] = Value::Null;
                                    continue;
                                }
                                v => v.as_i64().unwrap_or(1),
                            },
                            None => 1,
                        };
                        let target = if *kind == WinKind::Lag {
                            i as i64 - off
                        } else {
                            i as i64 + off
                        };
                        if call.ignore_nulls {
                            // Count only non-null values.
                            let step: i64 = if (*kind == WinKind::Lag) == (off >= 0) {
                                -1
                            } else {
                                1
                            };
                            let mut need = off.abs();
                            let mut p = i as i64;
                            let mut found = None;
                            if need == 0 {
                                found = Some(i);
                            }
                            while need > 0 {
                                p += step;
                                if p < 0 || p >= m as i64 {
                                    break;
                                }
                                let v = eval(&call.args[0], &rows[part[p as usize]], ctx)?;
                                if !v.is_null() {
                                    need -= 1;
                                    if need == 0 {
                                        found = Some(p as usize);
                                    }
                                }
                            }
                            match found {
                                Some(p) => eval(&call.args[0], &rows[part[p]], ctx)?,
                                None => match call.args.get(2) {
                                    Some(d) => eval(d, r, ctx)?,
                                    None => Value::Null,
                                },
                            }
                        } else if target >= 0 && (target as usize) < m {
                            eval(&call.args[0], &rows[part[target as usize]], ctx)?
                        } else {
                            match call.args.get(2) {
                                Some(d) => eval(d, r, ctx)?,
                                None => Value::Null,
                            }
                        }
                    }
                    WinKind::FirstValue | WinKind::LastValue | WinKind::NthValue => {
                        let (s, e) = frame_of(call, i, m, &peers, part, okeys, &offsets, ctx)?;
                        let positions: Vec<usize> =
                            (s..e).filter(|p| !excluded(call, i, *p, &peers)).collect();
                        let mut vals = Vec::new();
                        for p in &positions {
                            let v = eval(&call.args[0], &rows[part[*p]], ctx)?;
                            if call.ignore_nulls && v.is_null() {
                                continue;
                            }
                            vals.push(v);
                        }
                        match kind {
                            WinKind::FirstValue => vals.first().cloned().unwrap_or(Value::Null),
                            WinKind::LastValue => vals.last().cloned().unwrap_or(Value::Null),
                            _ => {
                                let nth = eval(&call.args[1], r, ctx)?;
                                if nth.is_null() {
                                    Value::Null
                                } else {
                                    let k = nth.as_i64().unwrap_or(0);
                                    if k <= 0 {
                                        return Err(Error::new(
                                            code::INVALID_ARGUMENT_FOR_NTH_VALUE,
                                            "argument of nth_value must be greater than zero",
                                        ));
                                    }
                                    vals.get(k as usize - 1).cloned().unwrap_or(Value::Null)
                                }
                            }
                        }
                    }
                };
                out[part[i]] = v;
            }
        }
        WinFunc::Agg(agg)
            if !matches!(call.frame.start, FrameBound::UnboundedPreceding)
                && !agg.distinct
                && agg.order_by.is_empty()
                && !agg.args.iter().chain(&agg.filter).any(|e| e.is_volatile())
                && agg.user.as_ref().is_some_and(|u| u.moving.is_some()) =>
        {
            let mv = agg.user.as_ref().and_then(|u| u.moving.as_deref()).unwrap();
            moving_aggregate(call, agg, mv, rows, part, &peers, okeys, &offsets, out, ctx)?;
        }
        WinFunc::Agg(agg) => {
            let incremental = matches!(call.frame.start, FrameBound::UnboundedPreceding)
                && call.frame.exclude == FrameExclude::NoOthers
                && !agg.distinct
                && agg.order_by.is_empty();
            let direct = eval_all(&agg.direct, &[], ctx)?;
            if incremental {
                let mut st = new_state(agg, direct.clone())?;
                let mut fed = 0usize;
                for i in 0..m {
                    let (_, e) = frame_of(call, i, m, &peers, part, okeys, &offsets, ctx)?;
                    while fed < e {
                        feed(agg, &mut st, &rows[part[fed]], ctx)?;
                        fed += 1;
                    }
                    // finish() consumes buffered state only for DISTINCT/ORDER BY (not here).
                    let v = snapshot_result(agg, &mut st, ctx)?;
                    out[part[i]] = v;
                }
            } else {
                for i in 0..m {
                    let (s, e) = frame_of(call, i, m, &peers, part, okeys, &offsets, ctx)?;
                    let mut st = new_state(agg, direct.clone())?;
                    for p in s..e {
                        if excluded(call, i, p, &peers) {
                            continue;
                        }
                        feed(agg, &mut st, &rows[part[p]], ctx)?;
                    }
                    out[part[i]] = finish(agg, st, ctx)?;
                }
            }
        }
    }
    let _ = DataType::Bool;
    let _ = Interval::new(0, 0, 0);
    Ok(())
}

/// A moving aggregate's transition state (WindowStatePerAggData).
struct MovingState {
    value: Value,
    /// transValueCount: rows aggregated and not yet removed.
    count: usize,
}

/// eval_windowaggregates for a user aggregate in moving-aggregate mode: rows leaving the
/// frame head are removed with the inverse transition (a NULL from it, or removing the last
/// row, restarts the aggregation).
#[allow(clippy::too_many_arguments)]
fn moving_aggregate(
    call: &WindowCall,
    agg: &crate::sql::plan::AggCall,
    mv: &crate::sql::plan::UserAgg,
    rows: &[Row],
    part: &[usize],
    peers: &[(usize, usize)],
    okeys: &[Vec<Value>],
    offsets: &(Option<Value>, Option<Value>),
    out: &mut [Value],
    ctx: &ExecCtx,
) -> Result<()> {
    let m = part.len();
    let init = |ctx: &ExecCtx| -> Result<MovingState> {
        Ok(MovingState {
            value: match &mv.init {
                Some(e) => eval(e, &[], ctx)?,
                None => Value::Null,
            },
            count: 0,
        })
    };
    // The aggregate's inputs for a row; None when FILTERed out.
    let inputs = |row: &Row| -> Result<Option<Vec<Value>>> {
        if let Some(f) = &agg.filter
            && !crate::sql::expr::eval_bool(f, row, ctx)?
        {
            return Ok(None);
        }
        Ok(Some(eval_all(&agg.args, row, ctx)?))
    };
    let call_with = |e: &crate::sql::expr::Expr, st: &Value, args: &[Value]| -> Result<Value> {
        let mut r = Vec::with_capacity(args.len() + 1);
        r.push(st.clone());
        r.extend_from_slice(args);
        eval(e, &r, ctx)
    };
    let advance = |st: &mut MovingState, row: &Row| -> Result<()> {
        let Some(args) = inputs(row)? else {
            return Ok(());
        };
        if mv.trans_strict {
            if args.iter().any(Value::is_null) {
                return Ok(());
            }
            if st.count == 0 && st.value.is_null() {
                st.value = args.first().cloned().unwrap_or(Value::Null);
                st.count = 1;
                return Ok(());
            }
            if st.value.is_null() {
                return Ok(());
            }
        }
        st.count += 1;
        let v = call_with(&mv.trans, &st.value, &args)?;
        if v.is_null() {
            return Err(Error::new(
                code::NULL_VALUE_NOT_ALLOWED,
                "moving-aggregate transition function must not return null",
            ));
        }
        st.value = v;
        Ok(())
    };
    // advance_windowaggregate_base: false when the inverse transition can't remove the row.
    let remove = |st: &mut MovingState, row: &Row| -> Result<bool> {
        let Some(args) = inputs(row)? else {
            return Ok(true);
        };
        let (inv, inv_strict) = mv.inverse.as_ref().expect("inverse transition");
        if *inv_strict && args.iter().any(Value::is_null) {
            return Ok(true);
        }
        if st.value.is_null() {
            return Err(crate::error::internal(
                "aggregate transition value is NULL before inverse transition",
            ));
        }
        if st.count == 1 {
            *st = init(ctx)?;
            return Ok(true);
        }
        let v = call_with(inv, &st.value, &args)?;
        if v.is_null() {
            return Ok(false);
        }
        st.count -= 1;
        st.value = v;
        Ok(true)
    };
    let finalize = |st: &MovingState| -> Result<Value> {
        match &mv.final_ {
            Some(_) if mv.final_strict && st.value.is_null() => Ok(Value::Null),
            Some(f) => eval(f, std::slice::from_ref(&st.value), ctx),
            None => Ok(st.value.clone()),
        }
    };
    let exclusion = call.frame.exclude != FrameExclude::NoOthers;
    let end_reuses = matches!(
        call.frame.end,
        FrameBound::UnboundedFollowing | FrameBound::CurrentRow
    );
    let mut st = init(ctx)?;
    let (mut base, mut upto) = (0usize, 0usize);
    let mut result = Value::Null;
    for i in 0..m {
        let (head, tail) = frame_of(call, i, m, peers, part, okeys, offsets, ctx)?;
        // The frame is the previous row's: its result stands.
        if i > 0 && base == head && end_reuses && !exclusion && base <= i && upto > i {
            out[part[i]] = result.clone();
            continue;
        }
        let mut restart = i == 0 || exclusion || upto <= head;
        while !restart && base < head {
            if !remove(&mut st, &rows[part[base]])? {
                restart = true;
            }
            base += 1;
        }
        base = head;
        if restart {
            st = init(ctx)?;
            upto = head;
        }
        while upto < m {
            if upto >= tail {
                break;
            }
            if !excluded(call, i, upto, peers) {
                advance(&mut st, &rows[part[upto]])?;
            }
            upto += 1;
        }
        result = finalize(&st)?;
        out[part[i]] = result.clone();
    }
    Ok(())
}

/// Result of an unbuffered aggregate state without consuming it.
fn snapshot_result(
    agg: &crate::sql::plan::AggCall,
    st: &mut super::agg::AggState,
    ctx: &ExecCtx,
) -> Result<Value> {
    super::agg::peek(agg, st, ctx)
}

/// The collations partitioning keys carry (as a COLLATE the binder put on them).
fn partition_collations(
    keys: &[crate::sql::expr::Expr],
    ctx: &ExecCtx,
) -> Vec<Option<std::sync::Arc<crate::types::collation::Collation>>> {
    let mut cat = None;
    keys.iter()
        .map(|k| match k {
            crate::sql::expr::Expr::Collate(_, name, _) => {
                let cat = cat.get_or_insert_with(|| ctx.catalog());
                crate::types::collation::by_name(cat, name).filter(|c| !c.bytewise())
            }
            _ => None,
        })
        .collect()
}
