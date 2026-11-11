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
use std::sync::Arc;

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
                        self.add_big(Numeric::from_parts(crate::types::decint::DecInt::from(acc), s));
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
                    None => self.add_big(Numeric::from_parts(crate::types::decint::DecInt::from(v), vs)),
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
                        None => self.add_big(Numeric::from_parts(crate::types::decint::DecInt::from(v), s)),
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
            .map(|(v, s)| Numeric::from_parts(crate::types::decint::DecInt::from(v), s));
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
    /// Groups kept as words (`Layout`), when every key and state is fixed-width.
    pub words: Option<Arc<Layout>>,
}

impl Spec {
    /// `key_types`: the keys' types, which decide whether the parts' groups are words.
    pub fn new(
        keys: Vec<K>,
        trim: Vec<bool>,
        aggs: Vec<(Option<K>, bool, Partial)>,
        seq: bool,
        key_types: &[crate::types::DataType],
    ) -> Spec {
        let words = Layout::of(&trim, &aggs, key_types).map(Arc::new);
        Spec { keys, trim, aggs, seq, words }
    }
}

/// A key a group's words hold: the type, whose values are the only ones taken (a row with
/// another kind of value goes to the serial path), each one word, equal words equal values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WordKey {
    Bool,
    Int2,
    Int4,
    Int8,
    Date,
    Timestamp,
    Timestamptz,
}

impl WordKey {
    fn of(t: &crate::types::DataType) -> Option<WordKey> {
        use crate::types::DataType as T;
        Some(match t {
            T::Bool => WordKey::Bool,
            T::Int2 => WordKey::Int2,
            T::Int4 => WordKey::Int4,
            T::Int8 => WordKey::Int8,
            T::Date => WordKey::Date,
            T::Timestamp(_) => WordKey::Timestamp,
            T::Timestamptz(_) => WordKey::Timestamptz,
            _ => return None,
        })
    }

    /// A value's word (None: NULL, or not this type's value).
    #[inline]
    fn word(self, v: &Value) -> Option<u64> {
        Some(match (self, v) {
            (WordKey::Bool, Value::Bool(b)) => *b as u64,
            (WordKey::Int2, Value::Int2(x)) => *x as i64 as u64,
            (WordKey::Int4, Value::Int4(x)) => *x as i64 as u64,
            (WordKey::Int8, Value::Int8(x)) => *x as u64,
            (WordKey::Date, Value::Date(x)) => *x as i64 as u64,
            (WordKey::Timestamp, Value::Timestamp(x)) => *x as u64,
            (WordKey::Timestamptz, Value::Timestamptz(x)) => *x as u64,
            _ => return None,
        })
    }

    fn value(self, w: u64) -> Value {
        match self {
            WordKey::Bool => Value::Bool(w != 0),
            WordKey::Int2 => Value::Int2(w as i64 as i16),
            WordKey::Int4 => Value::Int4(w as i64 as i32),
            WordKey::Int8 => Value::Int8(w as i64),
            WordKey::Date => Value::Date(w as i64 as i32),
            WordKey::Timestamp => Value::Timestamp(w as i64),
            WordKey::Timestamptz => Value::Timestamptz(w as i64),
        }
    }
}

/// A state a group's words hold: count `[n]`; integer sum `[has a value, sum]`; integer
/// average `[sum low, sum high, n]`; exact numeric sum `[sum low, sum high, scale, n]` (the
/// unscaled sum, at the largest scale seen; a value past an i128 goes to the serial path).
#[derive(Clone, Copy, Debug, PartialEq)]
enum WordState {
    Count,
    SumInt,
    AvgInt,
    SumNum,
}

impl WordState {
    fn width(self) -> usize {
        match self {
            WordState::Count => 1,
            WordState::SumInt => 2,
            WordState::AvgInt => 3,
            WordState::SumNum => 4,
        }
    }
}

fn wide(lo: u64, hi: u64) -> i128 {
    ((hi as u128) << 64 | lo as u128) as i128
}

/// A part's groups as words (`Groups::Words`): per group a word of which keys are NULL, a
/// word per key, then each state's words; no allocation of its own.
#[derive(Debug)]
pub struct Layout {
    keys: Vec<WordKey>,
    states: Vec<WordState>,
    /// Words a group.
    stride: usize,
}

impl Layout {
    fn of(trim: &[bool], aggs: &[(Option<K>, bool, Partial)], key_types: &[crate::types::DataType]) -> Option<Layout> {
        if key_types.is_empty() || key_types.len() > 64 || trim.iter().any(|t| *t) {
            return None;
        }
        let keys = key_types.iter().map(WordKey::of).collect::<Option<Vec<_>>>()?;
        let states = aggs
            .iter()
            .map(|(_, _, p)| match p {
                Partial::Count(0) => Some(WordState::Count),
                Partial::SumInt(None) => Some(WordState::SumInt),
                Partial::AvgInt(0, 0) => Some(WordState::AvgInt),
                Partial::SumNum(SumNum { big: None, small: None, n: 0 }) => Some(WordState::SumNum),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let stride = 1 + keys.len() + states.iter().map(|s| s.width()).sum::<usize>();
        Some(Layout { keys, states, stride })
    }
}

/// A part's groups, numbered by first appearance.
pub enum Groups {
    /// Each one's key and states as values.
    Values(Vec<(Vec<Value>, Vec<Partial>)>),
    /// Fixed-width keys and states in place (`Layout`).
    Words(Arc<Layout>, Vec<u64>),
}

impl Groups {
    pub fn len(&self) -> usize {
        match self {
            Groups::Values(g) => g.len(),
            Groups::Words(l, w) => w.len() / l.stride,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The groups in their order, as `GroupsIter::next` puts them.
    pub fn into_iter(self) -> GroupsIter {
        match self {
            Groups::Values(g) => GroupsIter::Values(g.into_iter()),
            Groups::Words(l, w) => GroupsIter::Words(l, w, 0),
        }
    }
}

/// `Groups::into_iter`.
pub enum GroupsIter {
    Values(std::vec::IntoIter<(Vec<Value>, Vec<Partial>)>),
    Words(Arc<Layout>, Vec<u64>, usize),
}

impl GroupsIter {
    /// The next group's key into `key` and states into `states` (both replaced), false
    /// after the last.
    pub fn next(&mut self, key: &mut Vec<Value>, states: &mut Vec<Partial>) -> bool {
        match self {
            GroupsIter::Values(it) => match it.next() {
                Some((k, s)) => {
                    *key = k;
                    *states = s;
                    true
                }
                None => false,
            },
            GroupsIter::Words(l, w, at) => {
                let Some(g) = w.get(*at..*at + l.stride) else { return false };
                *at += l.stride;
                key.clear();
                states.clear();
                let nulls = g[0];
                for (i, k) in l.keys.iter().enumerate() {
                    key.push(if nulls >> i & 1 == 1 { Value::Null } else { k.value(g[1 + i]) });
                }
                let mut o = 1 + l.keys.len();
                for s in &l.states {
                    states.push(match s {
                        WordState::Count => Partial::Count(g[o] as i64),
                        WordState::SumInt => Partial::SumInt((g[o] != 0).then_some(g[o + 1] as i64)),
                        WordState::AvgInt => Partial::AvgInt(wide(g[o], g[o + 1]), g[o + 2] as i64),
                        WordState::SumNum => {
                            let n = g[o + 3] as i64;
                            Partial::SumNum(SumNum {
                                big: None,
                                small: (n > 0).then(|| (wide(g[o], g[o + 1]), g[o + 2] as u32)),
                                n,
                            })
                        }
                    });
                    o += s.width();
                }
                true
            }
        }
    }
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
    pub groups: Groups,
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
    /// The part's groups (`Groups::Values`), each aggregated row's group in order (when
    /// `Spec::seq`), the rows read and those aggregated.
    groups: Vec<(Vec<Value>, Vec<Partial>)>,
    seq: Vec<u32>,
    nread: u64,
    rows: usize,
    /// `Spec::words`: the groups' words (`Groups::Words`), found by their NULLs and keys'
    /// words (`wtable`, of group numbers); a row's NULLs and key words, and its group's
    /// states after it (`wst`, the state words).
    words: Vec<u64>,
    wtable: hashbrown::HashTable<u32>,
    wkey: Vec<u64>,
    wst: Vec<u64>,
    hasher: super::fasthash::FastState,
    /// The keys and arguments a row computes (those that aren't columns or constants), and
    /// per argument its exact number when a numeric sum's was computed without a value
    /// (`K::num`).
    key: Vec<Value>,
    args: Vec<Value>,
    nums: Vec<Option<(i128, u32)>>,
    /// Per aggregate, a numeric sum's column read as its exact number (`Cols::num`: a
    /// stored row read in place makes no value; Some(None): NULL).
    cols: Vec<Option<Option<(i128, u32)>>>,
    /// An ungrouped aggregation whose arguments are columns (or none) and whose states are
    /// counts and exact sums: rows taken by `fast_row`, their columns read as numbers in
    /// place. `vals` holds a row's numbers (None: NULL).
    fast: bool,
    vals: Vec<Option<(i128, u32)>>,
    /// Groups by their key columns' bytes (`batch_row`; `index` decides on a miss), and a
    /// row's key as such bytes.
    bidx: hashbrown::HashMap<Box<[u8]>, u32, super::fasthash::FastState>,
    bkey: Vec<u8>,
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
            groups: Vec::new(),
            seq: Vec::new(),
            nread: 0,
            rows: 0,
            words: Vec::new(),
            wtable: hashbrown::HashTable::new(),
            wkey: Vec::new(),
            wst: Vec::new(),
            hasher: Default::default(),
            key: Vec::with_capacity(spec.keys.len()),
            args: Vec::with_capacity(spec.aggs.len()),
            nums: Vec::with_capacity(spec.aggs.len()),
            cols: Vec::with_capacity(spec.aggs.len()),
            fast: spec.keys.is_empty()
                && !spec.seq
                && spec.aggs.iter().all(|(a, _, p)| {
                    matches!(a, None | Some(K::Col(_)))
                        && matches!(p, Partial::Count(_) | Partial::SumInt(_) | Partial::AvgInt(..) | Partial::SumNum(_))
                }),
            vals: Vec::with_capacity(spec.aggs.len()),
            bidx: Default::default(),
            bkey: Vec::new(),
        }
    }

    /// `row` for a `fast` spec: Some(()) when it took the row, None when the general path
    /// decides (a column that isn't a small number, past the row's end, a state that can't
    /// take its value): nothing changed then.
    #[inline]
    fn fast_row<R: Cols + ?Sized>(&mut self, row: &R) -> Option<()> {
        self.vals.clear();
        for (a, skip, p) in &self.spec.aggs {
            let v = match (a, p) {
                // (count(*): a non-NULL stand-in.)
                (None, _) => Some((1, 0)),
                (Some(K::Col(i)), Partial::Count(_)) => (!row.col(*i)?.is_null()).then_some((1, 0)),
                (Some(K::Col(i)), _) => row.num(*i)?,
                _ => return None,
            };
            if v.is_none() && !*skip {
                return None;
            }
            self.vals.push(v);
        }
        if self.groups.is_empty() {
            let states = self.spec.aggs.iter().map(|(_, _, p)| p.clone()).collect();
            self.groups.push((Vec::new(), states));
        }
        let states = &mut self.groups[0].1;
        // (Checked before anything changes, as `row` does.)
        for (st, v) in states.iter().zip(&self.vals) {
            let Some((x, sc)) = *v else { continue };
            match st {
                Partial::SumInt(s) => {
                    let x = i64::try_from(x).ok().filter(|_| sc == 0)?;
                    s.unwrap_or(0).checked_add(x)?;
                }
                Partial::AvgInt(..) => {
                    i64::try_from(x).ok().filter(|_| sc == 0)?;
                }
                _ => {}
            }
        }
        for (st, v) in states.iter_mut().zip(&self.vals) {
            let Some((x, sc)) = *v else { continue };
            match st {
                Partial::Count(n) => *n += 1,
                Partial::SumInt(s) => *s = Some(s.unwrap_or(0) + x as i64),
                Partial::AvgInt(sum, n) => {
                    *sum += x;
                    *n += 1;
                }
                Partial::SumNum(sn) => sn.add_wide(x, sc),
                _ => {}
            }
        }
        self.rows += 1;
        Some(())
    }

    /// Rows read (the scan's: what its statistics count), aggregated or not.
    pub fn read(&mut self, n: u64) {
        self.nread += n;
    }

    /// A row that passed the filter: aggregated (true), or left for the serial path
    /// (false; the caller ends the part first: `take`). Its columns are read in place.
    pub fn row<R: Cols + ?Sized>(&mut self, row: &R) -> bool {
        if self.spec.words.is_some() {
            return self.row_words(row);
        }
        if self.fast && self.fast_row(row).is_some() {
            return true;
        }
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
        // (No grouping: the one group, once it is made.)
        let g = match self.spec.keys.is_empty() {
            true => (!self.groups.is_empty()).then_some(0),
            false => self.index.get(&kr).map(|g| *g as usize),
        };
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
                Some(g) => &self.groups[g].1[i],
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
                self.groups.push((own, states));
                let g = self.groups.len() - 1;
                self.index.insert(key, g as u32);
                g
            }
        };
        let mut at = 0;
        for (i, (a, skip, _)) in self.spec.aggs.iter().enumerate() {
            if let Some(c) = self.cols[i] {
                if !skipped(a, *skip, stand_in(&c)) {
                    match (&mut self.groups[g].1[i], c) {
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
                match (&mut self.groups[g].1[i], wide) {
                    (Partial::SumNum(sn), Some((n, sc))) => sn.add_wide(n, sc),
                    (st, _) => st.add(v),
                }
            }
        }
        if self.spec.seq {
            self.seq.push(g as u32);
        }
        self.rows += 1;
        true
    }

    /// `row` for a spec of words (`Layout`): its NULLs and key words into `wkey`, its
    /// aggregates' numbers into `vals` (None: a NULL skipped), then `add_words`. False,
    /// nothing changed: a value of another kind, or one a state doesn't take.
    fn row_words<R: Cols + ?Sized>(&mut self, row: &R) -> bool {
        let Some(l) = self.spec.words.clone() else { return false };
        self.wkey.clear();
        self.wkey.push(0);
        for (i, (k, wk)) in self.spec.keys.iter().zip(&l.keys).enumerate() {
            let tmp;
            let v = match k {
                K::Col(c) => match row.col(*c) {
                    Some(v) => v,
                    None => return false,
                },
                K::Const(v) => v,
                _ => match k.run(row) {
                    Some(v) => {
                        tmp = v;
                        &tmp
                    }
                    None => return false,
                },
            };
            if v.is_null() {
                self.wkey[0] |= 1 << i;
                self.wkey.push(0);
                continue;
            }
            let Some(w) = wk.word(v) else { return false };
            self.wkey.push(w);
        }
        self.vals.clear();
        for ((a, skip, _), st) in self.spec.aggs.iter().zip(&l.states) {
            let n = match (a, st) {
                // (count(*): a non-NULL stand-in.)
                (None, _) => Some((1, 0)),
                (Some(k), WordState::Count) => {
                    let null = match k {
                        K::Col(c) => match row.col(*c) {
                            Some(v) => v.is_null(),
                            None => return false,
                        },
                        K::Const(v) => v.is_null(),
                        _ => match k.run(row) {
                            Some(v) => v.is_null(),
                            None => return false,
                        },
                    };
                    (!null).then_some((1, 0))
                }
                (Some(k), _) => match k.num(row) {
                    Some(n) => n,
                    None => {
                        let v = match k {
                            K::Col(c) => match row.col(*c) {
                                Some(v) => v.clone(),
                                None => return false,
                            },
                            _ => match k.run(row) {
                                Some(v) => v,
                                None => return false,
                            },
                        };
                        if v.is_null() {
                            None
                        } else {
                            match super::kernel::small_num(&v) {
                                Some(n) => Some(n),
                                None => return false,
                            }
                        }
                    }
                },
            };
            // (A NULL an aggregate doesn't skip: the serial path adds it as only it can.)
            if n.is_none() && !*skip {
                return false;
            }
            self.vals.push(n);
        }
        self.add_words()
    }

    /// The row whose key words are in `wkey` and aggregates' numbers in `vals` added to its
    /// group (made after the last when new), when every state takes its number; else
    /// false, nothing changed.
    fn add_words(&mut self) -> bool {
        use std::hash::BuildHasher;
        let Some(l) = self.spec.words.as_deref() else { return false };
        let (stride, nk) = (l.stride, self.wkey.len());
        let words = &self.words;
        let h = self.hasher.hash_one(&self.wkey[..]);
        let key = &self.wkey[..];
        let g = self.wtable.find(h, |&g| words[g as usize * stride..][..nk] == *key).map(|g| *g as usize);
        // (The group's states after the row, in `wst`: checked before anything changes.)
        self.wst.clear();
        match g {
            Some(g) => self.wst.extend_from_slice(&words[g * stride + nk..(g + 1) * stride]),
            None => self.wst.resize(stride - nk, 0),
        }
        let mut o = 0;
        for (s, v) in l.states.iter().zip(&self.vals) {
            let w = &mut self.wst[o..o + s.width()];
            o += s.width();
            let Some((x, sc)) = *v else { continue };
            match s {
                WordState::Count => w[0] += 1,
                WordState::SumInt => {
                    let Some(x) = i64::try_from(x).ok().filter(|_| sc == 0) else { return false };
                    let Some(t) = (w[1] as i64).checked_add(x) else { return false };
                    (w[0], w[1]) = (1, t as u64);
                }
                WordState::AvgInt => {
                    if sc != 0 || i64::try_from(x).is_err() {
                        return false;
                    }
                    let Some(t) = wide(w[0], w[1]).checked_add(x) else { return false };
                    (w[0], w[1], w[2]) = (t as u64, (t >> 64) as u64, w[2] + 1);
                }
                WordState::SumNum => {
                    // (SumNum::add_wide's sum, at the larger scale; past an i128: not taken.)
                    let (t, ns) = if w[3] == 0 {
                        (x, sc)
                    } else {
                        let (acc, s) = (wide(w[0], w[1]), w[2] as u32);
                        let ns = s.max(sc);
                        let r = (|| {
                            let a = acc.checked_mul(10i128.checked_pow(ns - s)?)?;
                            let b = x.checked_mul(10i128.checked_pow(ns - sc)?)?;
                            a.checked_add(b)
                        })();
                        match r {
                            Some(t) => (t, ns),
                            None => return false,
                        }
                    };
                    (w[0], w[1], w[2], w[3]) = (t as u64, (t >> 64) as u64, ns as u64, w[3] + 1);
                }
            }
        }
        let g = match g {
            Some(g) => {
                self.words[g * stride + nk..(g + 1) * stride].copy_from_slice(&self.wst);
                g
            }
            None => {
                let g = self.words.len() / stride;
                self.words.extend_from_slice(&self.wkey);
                self.words.extend_from_slice(&self.wst);
                let (words, hasher) = (&self.words, &self.hasher);
                self.wtable.insert_unique(h, g as u32, |&g| hasher.hash_one(&words[g as usize * stride..][..nk]));
                g
            }
        };
        if self.spec.seq {
            self.seq.push(g as u32);
        }
        self.rows += 1;
        true
    }

    /// Rows aggregated in the part so far.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Row `r` of the column batch `b` (one that passed the filter, not odd) aggregated by
    /// `plan` (`vector::AggPlan`, compiled from this spec), as `row` would aggregate it
    /// (true), or nothing changed (false: the caller hands the row to `row`, in its place).
    pub fn batch_row(
        &mut self,
        plan: &super::vector::AggPlan,
        b: &crate::storage::cols::ColBatch,
        nums: &[Vec<super::vector::Num>],
        r: usize,
    ) -> bool {
        use super::vector::AggArg;
        self.vals.clear();
        for (i, (arg, (_, skip, _))) in plan.args.iter().zip(&self.spec.aggs).enumerate() {
            let v = match arg {
                // (count(*)'s: a non-NULL stand-in.)
                AggArg::Star => Some((1, 0)),
                AggArg::Count(c) => (!b.cols[*c].as_ref().is_none_or(|c| c.nulls[r])).then_some((1, 0)),
                AggArg::Num(_) => match nums[i][r] {
                    Some(v) => v,
                    None => return false,
                },
            };
            // (A NULL an aggregate doesn't skip: `row` adds it as only it can.)
            if v.is_none() && !*skip {
                return false;
            }
            self.vals.push(v);
        }
        if let Some(l) = &self.spec.words {
            self.wkey.clear();
            self.wkey.push(0);
            for (i, ((c, ty), k)) in plan.keys.iter().zip(&plan.key_types).zip(&l.keys).enumerate() {
                let Some(col) = &b.cols[*c] else { return false };
                if col.nulls[r] {
                    self.wkey[0] |= 1 << i;
                    self.wkey.push(0);
                    continue;
                }
                let Some(w) = col.value(b, r, ty).ok().and_then(|v| k.word(&v)) else { return false };
                self.wkey.push(w);
            }
            return self.add_words();
        }
        let g = match plan.keys.is_empty() {
            true => (!self.groups.is_empty()).then_some(0),
            false => {
                if !self.batch_key(plan, b, r) {
                    return false;
                }
                match self.bidx.get(&self.bkey[..]) {
                    Some(g) => Some(*g as usize),
                    None => match self.batch_values(plan, b, r) {
                        Some((key, _)) => self.index.get(&key[..]).map(|g| *g as usize),
                        None => return false,
                    },
                }
            }
        };
        // (Checked before anything changes, as `row` does.)
        for (i, v) in self.vals.iter().enumerate() {
            let Some((x, sc)) = *v else { continue };
            let st = match g {
                Some(g) => &self.groups[g].1[i],
                None => &self.spec.aggs[i].2,
            };
            match st {
                Partial::SumInt(s) => {
                    let Some(x) = i64::try_from(x).ok().filter(|_| sc == 0) else { return false };
                    if s.unwrap_or(0).checked_add(x).is_none() {
                        return false;
                    }
                }
                Partial::AvgInt(..) if sc != 0 || i64::try_from(x).is_err() => return false,
                _ => {}
            }
        }
        let g = match g {
            Some(g) => g,
            None => {
                let states = self.spec.aggs.iter().map(|(_, _, p)| p.clone()).collect();
                let g = self.groups.len();
                if plan.keys.is_empty() {
                    self.groups.push((Vec::new(), states));
                } else {
                    let Some((key, own)) = self.batch_values(plan, b, r) else { return false };
                    self.groups.push((own, states));
                    self.index.insert(key.into_boxed_slice(), g as u32);
                }
                g
            }
        };
        if !plan.keys.is_empty() && !self.bidx.contains_key(&self.bkey[..]) {
            self.bidx.insert(self.bkey.as_slice().into(), g as u32);
        }
        for (st, v) in self.groups[g].1.iter_mut().zip(&self.vals) {
            let Some((x, sc)) = *v else { continue };
            match st {
                Partial::Count(n) => *n += 1,
                Partial::SumInt(s) => *s = Some(s.unwrap_or(0) + x as i64),
                Partial::AvgInt(sum, n) => {
                    *sum += x;
                    *n += 1;
                }
                Partial::SumNum(sn) => sn.add_wide(x, sc),
                _ => {}
            }
        }
        if self.spec.seq {
            self.seq.push(g as u32);
        }
        self.rows += 1;
        true
    }

    /// Row `r`'s group key as bytes into `bkey` (bpchar keys trimmed): each value's kind
    /// and its bytes, so equal bytes are equal keys (false: a key of a kind it doesn't
    /// take).
    fn batch_key(&mut self, plan: &super::vector::AggPlan, b: &crate::storage::cols::ColBatch, r: usize) -> bool {
        use crate::storage::cols::ColData as D;
        let k = &mut self.bkey;
        k.clear();
        for (c, trim) in plan.keys.iter().zip(&self.spec.trim) {
            let Some(col) = &b.cols[*c] else { return false };
            if col.nulls[r] {
                k.push(0);
                continue;
            }
            match &col.data {
                D::I32(v) => k.extend(std::iter::once(2).chain(v[r].to_le_bytes())),
                D::I64(v) => k.extend(std::iter::once(3).chain(v[r].to_le_bytes())),
                D::Date(v) => k.extend(std::iter::once(4).chain(v[r].to_le_bytes())),
                D::Bool(v) => k.extend([7, v[r] as u8]),
                D::Bytes(_) => {
                    let mut t = col.bytes(b, r);
                    if *trim {
                        while let [rest @ .., b' '] = t {
                            t = rest;
                        }
                    }
                    k.push(8);
                    k.extend((t.len() as u32).to_le_bytes());
                    k.extend_from_slice(t);
                }
                _ => return false,
            }
        }
        true
    }

    /// Row `r`'s group key as values: as groups compare it (bpchar keys trimmed) and the
    /// group's own (its first row's values). None: a value it doesn't make.
    fn batch_values(&self, plan: &super::vector::AggPlan, b: &crate::storage::cols::ColBatch, r: usize) -> Option<(Vec<Value>, Vec<Value>)> {
        let mut key = Vec::with_capacity(plan.keys.len());
        let mut own = Vec::with_capacity(plan.keys.len());
        for ((c, trim), ty) in plan.keys.iter().zip(&self.spec.trim).zip(&plan.key_types) {
            let v = b.cols[*c].as_ref()?.value(b, r, ty).ok()?;
            key.push(if *trim { trimmed(v.clone()) } else { v.clone() });
            own.push(v);
        }
        Some((key, own))
    }

    /// The part so far (the next one starts empty).
    pub fn take(&mut self) -> Option<Part> {
        if self.nread == 0 && self.rows == 0 {
            return None;
        }
        self.index.clear();
        self.bidx.clear();
        self.wtable.clear();
        let groups = match &self.spec.words {
            Some(l) => Groups::Words(l.clone(), std::mem::take(&mut self.words)),
            None => Groups::Values(std::mem::take(&mut self.groups)),
        };
        let part = Part { groups, seq: std::mem::take(&mut self.seq), read: self.nread, rows: self.rows };
        (self.nread, self.rows) = (0, 0);
        Some(part)
    }
}
