//! Partial aggregation off the executor's thread: a plain GROUP BY over a parallel scan
//! (`parscan`) whose keys and arguments compile to kernels and whose aggregates are exact
//! (counts, numeric and integer sums and averages, min / max by the type's order) runs as
//! PostgreSQL's parallel plan does, Partial Aggregates in the workers and the Finalize on
//! the executor's thread, with nothing observable changed: exact aggregates come out the
//! same however their rows are split, groups are numbered and looked up in the serial
//! scan's row order (the hash table's order follows it), and a row a kernel can't do comes
//! to the executor's thread as a row, in its place, for the serial path.

use super::Row;
use super::kernel::{Cols, K};
use crate::types::Value;
use crate::types::numeric::Numeric;
use std::cmp::Ordering;

/// An aggregate's partial state (an `Accum` that can be split: `Accum::partial`).
#[derive(Clone, Debug)]
pub enum Partial {
    Count(i64),
    SumNum(SumNum),
    SumInt(Option<i64>),
    AvgInt(i128, i64),
    /// min / max by the type's order (`Best`).
    Best(Best),
    /// A float aggregate's values in their order (it sums them on the executor's thread,
    /// in the serial order: float sums depend on it).
    Floats(Vec<f64>),
}

/// min / max's state without an ordering operator of its own: the best value so far, and
/// how the type compares and keeps ties (`replaces`).
#[derive(Clone, Debug)]
pub struct Best {
    pub best: Option<Value>,
    pub max: bool,
    /// bpchar: compared without trailing spaces.
    pub bpchar: bool,
    /// A tie keeps the old value (bpchar's and tid's X_larger / X_smaller); others the new.
    pub keep_old: bool,
}

impl Best {
    /// Whether `v` replaces the best so far, `b` (MinMax's rule without an operator).
    pub fn replaces(&self, v: &Value, b: &Value) -> bool {
        let ord = match (self.bpchar, v, b) {
            (true, Value::Text(x), Value::Text(y)) => x.trim_end_matches(' ').cmp(y.trim_end_matches(' ')),
            _ => v.cmp_total(b),
        };
        (ord == Ordering::Equal && !self.keep_old)
            || if self.max { ord == Ordering::Greater } else { ord == Ordering::Less }
    }

    pub fn add(&mut self, v: &Value) {
        if self.best.as_ref().is_none_or(|b| self.replaces(v, b)) {
            self.best = Some(v.clone());
        }
    }
}

/// sum(numeric / int8)'s state: values fitting an i64 summed exactly in `small` (an i128
/// at the largest scale seen), the rest (and what overflowed it) in `big`.
#[derive(Clone, Debug, Default)]
pub struct SumNum {
    pub big: Option<Numeric>,
    pub small: Option<(i128, u32)>,
    pub n: i64,
}

/// `acc` (at scale `s`) plus `v` (at scale `vs`), at the larger scale; None: overflow.
fn add_small(acc: i128, s: u32, v: i64, vs: u32) -> Option<(i128, u32)> {
    let ns = s.max(vs);
    let a = acc.checked_mul(10i128.checked_pow(ns - s)?)?;
    let b = (v as i128).checked_mul(10i128.checked_pow(ns - vs)?)?;
    Some((a.checked_add(b)?, ns))
}

/// A value as sum(numeric)'s accumulation takes it (`agg::to_num`).
pub fn to_num(v: &Value) -> Numeric {
    match v {
        Value::Numeric(n) => (**n).clone(),
        Value::Float4(f) => Numeric::from_f32(*f),
        Value::Float8(f) => Numeric::from_f64(*f),
        v => Numeric::from_i64(v.as_i64().unwrap_or(0)),
    }
}

impl SumNum {
    fn add_big(&mut self, v: Numeric) {
        self.big = Some(match self.big.take() {
            None => v,
            Some(s) => s.add(&v),
        });
    }

    pub fn add(&mut self, v: &Value) {
        use num_traits::ToPrimitive;
        let small = match v {
            Value::Numeric(x) => match &**x {
                Numeric::Finite { int, scale } => int.to_i64().map(|i| (i, *scale)),
                _ => None,
            },
            Value::Int8(i) => Some((*i, 0)),
            _ => None,
        };
        match small {
            Some((i, vs)) => match self.small {
                None => self.small = Some((i as i128, vs)),
                Some((acc, s)) => match add_small(acc, s, i, vs) {
                    Some(r) => self.small = Some(r),
                    None => {
                        // (Past the i128: what it holds goes to the exact sum.)
                        self.small = Some((i as i128, vs));
                        self.add_big(Numeric::from_parts(num_bigint::BigInt::from(acc), s));
                    }
                },
            },
            None => self.add_big(to_num(v)),
        }
        self.n += 1;
    }

    /// A value given as its unscaled i128 `v` and scale `vs` added (`add`'s sum, exactly).
    pub fn add_wide(&mut self, v: i128, vs: u32) {
        match self.small {
            None => self.small = Some((v, vs)),
            Some((acc, s)) => {
                let ns = s.max(vs);
                let r = (|| {
                    let a = acc.checked_mul(10i128.checked_pow(ns - s)?)?;
                    let b = v.checked_mul(10i128.checked_pow(ns - vs)?)?;
                    a.checked_add(b)
                })();
                match r {
                    Some(r) => self.small = Some((r, ns)),
                    None => self.add_big(Numeric::from_parts(num_bigint::BigInt::from(v), vs)),
                }
            }
        }
        self.n += 1;
    }

    /// Another part's rows added (an exact sum: the same in any order).
    pub fn merge(&mut self, o: SumNum) {
        if let Some((v, s)) = o.small {
            match self.small {
                None => self.small = Some((v, s)),
                Some((acc, ss)) => {
                    let ns = s.max(ss);
                    let r = (|| {
                        let a = acc.checked_mul(10i128.checked_pow(ns - ss)?)?;
                        let b = v.checked_mul(10i128.checked_pow(ns - s)?)?;
                        a.checked_add(b)
                    })();
                    match r {
                        Some(r) => self.small = Some((r, ns)),
                        None => self.add_big(Numeric::from_parts(num_bigint::BigInt::from(v), s)),
                    }
                }
            }
        }
        if let Some(b) = o.big {
            self.add_big(b);
        }
        self.n += o.n;
    }

    /// The whole sum (None: no rows).
    pub fn total(&self) -> Option<Numeric> {
        let small = self
            .small
            .map(|(v, s)| Numeric::from_parts(num_bigint::BigInt::from(v), s));
        match (&self.big, small) {
            (None, None) => None,
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b),
            (Some(a), Some(b)) => Some(a.add(&b)),
        }
    }
}

impl Partial {
    /// Whether `add` takes `v` (else the row goes to the serial path, which fails on it
    /// or compares it as only it can).
    pub fn takes(&self, v: &Value) -> bool {
        match self {
            Partial::SumInt(s) => s.unwrap_or(0).checked_add(v.as_i64().unwrap_or(0)).is_some(),
            Partial::Best(..) => !matches!(v, Value::Array(_)),
            _ => true,
        }
    }

    /// One row's argument (one `takes`).
    pub fn add(&mut self, v: &Value) {
        match self {
            Partial::Count(n) => *n += 1,
            Partial::SumNum(s) => s.add(v),
            Partial::SumInt(s) => {
                let x = v.as_i64().unwrap_or(0);
                *s = Some(s.unwrap_or(0).saturating_add(x));
            }
            Partial::AvgInt(sum, n) => {
                *sum += v.as_i64().unwrap_or(0) as i128;
                *n += 1;
            }
            Partial::Floats(xs) => xs.push(v.as_f64().unwrap_or(0.0)),
            Partial::Best(b) => b.add(v),
        }
    }
}

/// What the workers compute: the group keys and each aggregate's argument (None:
/// count(*)), whether it skips NULLs, and its empty partial state.
pub struct Spec {
    pub keys: Vec<K>,
    /// Per key, whether it is a bpchar: grouped without its trailing spaces (bpchareq), the
    /// group keeping its first row's value.
    pub trim: Vec<bool>,
    pub aggs: Vec<(Option<K>, bool, Partial)>,
    /// Whether the executor's thread reads `Part::seq` (the hash table's order model).
    pub seq: bool,
}

/// A bpchar key's value as its groups compare it.
fn trimmed(v: Value) -> Value {
    match v {
        Value::Text(s) if s.ends_with(' ') => Value::text(s.trim_end_matches(' ')),
        v => v,
    }
}

/// A part of a chunk's rows aggregated: its groups (keys and states, numbered by first
/// appearance), each aggregated row's group in order, and the rows read (the filter's
/// dropped ones too).
pub struct Part {
    pub groups: Vec<(Vec<Value>, Vec<Partial>)>,
    pub seq: Vec<u32>,
    pub read: u64,
    /// The rows aggregated.
    pub rows: usize,
}

/// A chunk's rows as they come: aggregated parts, and rows left to the serial path (each
/// with the pipeline stages it went through, `pipe`, and whether the scan's filter is
/// done with it).
pub enum Seg {
    Part(Part),
    Rows(Vec<(Row, u8, bool)>),
}

/// Aggregates a run of rows (a worker's chunk) into parts, handing rows it can't do over
/// as rows.
pub struct Partials<'a> {
    spec: &'a Spec,
    index: hashbrown::HashMap<Box<[Value]>, u32, super::fasthash::FastState>,
    part: Part,
    /// The keys and arguments a row computes (those that aren't columns or constants), and
    /// per argument its exact number when a numeric sum's was computed without a value
    /// (`K::num`).
    key: Vec<Value>,
    args: Vec<Value>,
    nums: Vec<Option<(i128, u32)>>,
    /// Per aggregate, a numeric sum's column read as its exact number (`Cols::num`: a
    /// stored row read in place makes no value; Some(None): NULL).
    cols: Vec<Option<Option<(i128, u32)>>>,
}

/// A kernel's value over `row`: a column's or constant's in place, else the next of the
/// values computed before (`computed`, at `*at`).
#[inline]
fn value<'v, R: Cols + ?Sized>(k: &'v K, row: &'v R, computed: &'v [Value], at: &mut usize) -> &'v Value {
    match k {
        K::Col(i) => row.col(*i).unwrap_or(&Value::Null),
        K::Const(v) => v,
        _ => {
            *at += 1;
            &computed[*at - 1]
        }
    }
}

/// An aggregate's argument over `row` (`value`; count(*)'s: NULL).
#[inline]
fn arg<'v, R: Cols + ?Sized>(a: &'v Option<K>, row: &'v R, computed: &'v [Value], at: &mut usize) -> &'v Value {
    match a {
        Some(k) => value(k, row, computed, at),
        None => &Value::Null,
    }
}

/// Whether a kernel's value needs computing (not a column or a constant).
fn computed(k: &K) -> bool {
    !matches!(k, K::Col(_) | K::Const(_))
}

/// A row's group key as its groups compare it (its columns not copied; bpchar keys
/// trimmed, computed), to look its group up by.
struct KeyRef<'x, R: ?Sized> {
    keys: &'x [K],
    trim: &'x [bool],
    row: &'x R,
    computed: &'x [Value],
}

impl<R: Cols + ?Sized> KeyRef<'_, R> {
    fn each(&self, mut f: impl FnMut(&Value) -> bool) -> bool {
        let mut at = 0;
        self.keys.iter().zip(self.trim).all(|(k, t)| {
            if *t {
                at += 1;
                f(&self.computed[at - 1])
            } else {
                f(value(k, self.row, self.computed, &mut at))
            }
        })
    }
}

impl<R: Cols + ?Sized> std::hash::Hash for KeyRef<'_, R> {
    /// As the key's `[Value]` hashes (its length, then each value).
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(self.keys.len());
        self.each(|v| {
            v.hash(state);
            true
        });
    }
}

impl<R: Cols + ?Sized> hashbrown::Equivalent<Box<[Value]>> for KeyRef<'_, R> {
    fn equivalent(&self, key: &Box<[Value]>) -> bool {
        let mut i = 0;
        key.len() == self.keys.len()
            && self.each(|v| {
                i += 1;
                *v == key[i - 1]
            })
    }
}

impl<'a> Partials<'a> {
    pub fn new(spec: &'a Spec) -> Self {
        Partials {
            spec,
            index: Default::default(),
            part: Part { groups: Vec::new(), seq: Vec::new(), read: 0, rows: 0 },
            key: Vec::with_capacity(spec.keys.len()),
            args: Vec::with_capacity(spec.aggs.len()),
            nums: Vec::with_capacity(spec.aggs.len()),
            cols: Vec::with_capacity(spec.aggs.len()),
        }
    }

    /// Rows read (the scan's: what its statistics count), aggregated or not.
    pub fn read(&mut self, n: u64) {
        self.part.read += n;
    }

    /// A row that passed the filter: aggregated (true), or left for the serial path
    /// (false; the caller ends the part first: `take`). Its columns are read in place.
    pub fn row<R: Cols + ?Sized>(&mut self, row: &R) -> bool {
        self.key.clear();
        for (k, t) in self.spec.keys.iter().zip(&self.spec.trim) {
            if !computed(k) && !*t {
                continue;
            }
            match k.run(row) {
                Some(v) => self.key.push(if *t { trimmed(v) } else { v }),
                None => return false,
            }
        }
        self.args.clear();
        self.nums.clear();
        self.cols.clear();
        for (a, _, p) in self.spec.aggs.iter() {
            self.cols.push(match (a, p) {
                (Some(a @ K::Col(_)), Partial::SumNum(_)) => a.num(row),
                _ => None,
            });
        }
        for (a, _, p) in self.spec.aggs.iter() {
            let Some(a) = a.as_ref().filter(|a| computed(a)) else { continue };
            // (A numeric sum's argument summed exactly as it is computed: no value made.)
            if matches!(p, Partial::SumNum(_))
                && matches!(a, K::Num { .. })
                && let Some(r) = a.num(row)
            {
                // (A non-NULL stand-in: `add_wide` takes the number.)
                self.args.push(if r.is_some() { Value::Bool(true) } else { Value::Null });
                self.nums.push(r);
                continue;
            }
            match a.run(row) {
                Some(v) => self.args.push(v),
                None => return false,
            }
            self.nums.push(None);
        }
        // (Columns past the row's end: left to the serial path, as `run` would.)
        let present = |k: &K| !matches!(k, K::Col(i) if row.col(*i).is_none());
        let args = self.spec.aggs.iter().zip(&self.cols).filter(|(_, c)| c.is_none());
        if !self.spec.keys.iter().all(present) || !args.filter_map(|(a, _)| a.0.as_ref()).all(present) {
            return false;
        }
        // (A column's number stands in for its value: NULL or not is what's read of it.)
        let stand_in = |c: &Option<(i128, u32)>| if c.is_some() { &Value::Bool(true) } else { &Value::Null };
        let kr = KeyRef { keys: &self.spec.keys, trim: &self.spec.trim, row, computed: &self.key };
        let g = self.index.get(&kr).map(|g| *g as usize);
        // (A state that can't take its value leaves the whole row to the serial path:
        // checked before anything changes.)
        let skipped = |a: &Option<K>, skip: bool, v: &Value| a.is_some() && skip && v.is_null();
        let mut at = 0;
        for (i, (a, skip, empty)) in self.spec.aggs.iter().enumerate() {
            let v = match &self.cols[i] {
                Some(c) => stand_in(c),
                None => arg(a, row, &self.args, &mut at),
            };
            let st = match g {
                Some(g) => &self.part.groups[g].1[i],
                None => empty,
            };
            if !skipped(a, *skip, v) && !st.takes(v) {
                return false;
            }
        }
        let g = match g {
            Some(g) => g,
            None => {
                let mut key = Vec::with_capacity(self.spec.keys.len());
                kr.each(|v| {
                    key.push(v.clone());
                    true
                });
                // (The group's own key: its first row's values, bpchars untrimmed.)
                let mut own = key.clone();
                for ((k, t), v) in self.spec.keys.iter().zip(&self.spec.trim).zip(own.iter_mut()) {
                    if *t {
                        *v = k.run(row).unwrap_or(Value::Null);
                    }
                }
                let key: Box<[Value]> = key.into_boxed_slice();
                let states = self.spec.aggs.iter().map(|(_, _, p)| p.clone()).collect();
                self.part.groups.push((own, states));
                let g = self.part.groups.len() - 1;
                self.index.insert(key, g as u32);
                g
            }
        };
        let mut at = 0;
        for (i, (a, skip, _)) in self.spec.aggs.iter().enumerate() {
            if let Some(c) = self.cols[i] {
                if !skipped(a, *skip, stand_in(&c)) {
                    match (&mut self.part.groups[g].1[i], c) {
                        (Partial::SumNum(sn), Some((n, sc))) => sn.add_wide(n, sc),
                        (st, _) => st.add(stand_in(&c)),
                    }
                }
                continue;
            }
            let ci = at;
            let v = arg(a, row, &self.args, &mut at);
            if !skipped(a, *skip, v) {
                let wide = if at > ci { self.nums[ci] } else { None };
                match (&mut self.part.groups[g].1[i], wide) {
                    (Partial::SumNum(sn), Some((n, sc))) => sn.add_wide(n, sc),
                    (st, _) => st.add(v),
                }
            }
        }
        if self.spec.seq {
            self.part.seq.push(g as u32);
        }
        self.part.rows += 1;
        true
    }

    /// Rows aggregated in the part so far.
    pub fn rows(&self) -> usize {
        self.part.rows
    }

    /// The part so far (the next one starts empty).
    pub fn take(&mut self) -> Option<Part> {
        if self.part.read == 0 && self.part.rows == 0 {
            return None;
        }
        self.index.clear();
        Some(std::mem::replace(&mut self.part, Part { groups: Vec::new(), seq: Vec::new(), read: 0, rows: 0 }))
    }
}
