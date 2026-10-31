//! Hash join tables, and the probes of them a parallel scan's workers run over its rows
//! (PostgreSQL's Parallel Hash Join, its probe side): a hash join over a parallel scan
//! builds its table on the executor's thread as before, then shares it, read-only, with
//! the scan's workers, which probe it with every row they read and filter, in the scan's
//! order. Joins above joins stack their probes the same way (`Probe` stages, bottom
//! first), and an aggregation above them can aggregate the joined rows in parts
//! (`partial`). The rows come to the executor's thread in the serial order, each tagged
//! with how many stages it has been through: a row a kernel left to `eval` (a key, a join
//! condition) comes out of the stage before, and the join node it belongs to runs it as
//! the serial join does, in its place.

use super::Row;
use super::kernel::K;
use super::partial::Part;
use crate::sql::expr::Expr;
use crate::sql::plan::JoinKind;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// How a join key's values are made comparable (equality's view of them, so equal values
/// hash alike): integers of any width as int8, floats as float8, an integer meeting a
/// numeric as a numeric, bpchar without its trailing spaces (bpchareq).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Norm {
    /// Integers widened to int8 (the default).
    Int,
    Float,
    Numeric,
    Bpchar,
}

/// The numeric family a key type belongs to: 0 integers, 1 floats, 2 numeric.
fn family(ty: &DataType) -> Option<u8> {
    match ty {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => Some(0),
        DataType::Float4 | DataType::Float8 => Some(1),
        DataType::Numeric(_) => Some(2),
        _ => None,
    }
}

/// How one side's key of type `own`, compared with the other side's of type `other`, is
/// normalised. Both sides of an integer, float or numeric pair get the same: values
/// that are equal compare and hash alike whichever side they come from (an int8 meeting
/// a numeric only through a hash collision can't happen).
pub fn norm_for(own: &DataType, other: &DataType) -> Norm {
    if matches!(own, DataType::Bpchar(_)) {
        return Norm::Bpchar;
    }
    match (family(own), family(other)) {
        (Some(a), Some(b)) => match a.max(b) {
            0 => Norm::Int,
            // (An integer or numeric compared with a float is compared as a float.)
            _ if a == 1 || b == 1 => Norm::Float,
            _ => Norm::Numeric,
        },
        (Some(1), None) => Norm::Float,
        (Some(2), None) => Norm::Numeric,
        _ => Norm::Int,
    }
}

/// Per key, how each side's values are normalised: (outer side's, inner side's).
pub fn norms(lkeys: &[Expr], rkeys: &[Expr]) -> (Vec<Norm>, Vec<Norm>) {
    lkeys
        .iter()
        .zip(rkeys)
        .map(|(l, r)| {
            let (lt, rt) = (l.ty(), r.ty());
            (norm_for(&lt, &rt), norm_for(&rt, &lt))
        })
        .unzip()
}

/// `v` as join keys compare it.
pub fn norm(v: Value, n: Norm) -> Value {
    match (n, v) {
        (_, Value::Null) => Value::Null,
        (Norm::Int, Value::Int2(x)) => Value::Int8(x as i64),
        (Norm::Int, Value::Int4(x)) => Value::Int8(x as i64),
        (Norm::Float, Value::Float4(f)) => Value::Float8(f as f64),
        (Norm::Float, v @ (Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Numeric(_))) => {
            Value::Float8(v.as_f64().unwrap_or(0.0))
        }
        (Norm::Numeric, v @ (Value::Int2(_) | Value::Int4(_) | Value::Int8(_))) => Value::Numeric(Arc::new(
            crate::types::numeric::Numeric::from_i64(v.as_i64().unwrap_or(0)),
        )),
        (Norm::Bpchar, Value::Text(s)) if s.ends_with(' ') => Value::text(s.trim_end_matches(' ')),
        (_, Value::Int2(x)) => Value::Int8(x as i64),
        (_, Value::Int4(x)) => Value::Int8(x as i64),
        (_, v) => v,
    }
}

/// A key's candidates: a range of `Built::cands`.
type Span = (u32, u32);

/// The table's keys: one int8 key in a map of its own (no key vectors to hash, compare
/// or chase), others as vectors.
enum KeyMap {
    Int(super::fasthash::HashMap<i64, Span>),
    Vals(super::fasthash::HashMap<Box<[Value]>, Span>),
}

/// A hash join's inner rows in their hash table: the bucket chains Postgres builds
/// (`pghash::JoinTable`, for the order unmatched rows come out in) and, per key, the rows
/// in their chain's order (the most recently inserted first; a merge join's: in their
/// order). Read-only once built: shared with the workers that probe it.
pub struct Built {
    pub rows: Vec<Row>,
    map: KeyMap,
    cands: Vec<u32>,
    pub table: super::pghash::JoinTable,
    /// Each inner column's place in a compacted row (`compact`; `u32::MAX`: not kept).
    cols: Option<Box<[u32]>>,
}

impl Built {
    /// Built from `rows`, each with its normalised key (None: a NULL key, never matched),
    /// and the chains model; `pos[i]`: row i's place in its chain; `merge`: equal rows in
    /// their order instead.
    pub fn new(rows: Vec<Row>, keys: Vec<(Vec<Value>, usize)>, table: super::pghash::JoinTable, merge: bool) -> Built {
        let mut pos = vec![usize::MAX; rows.len()];
        for (p, i) in table.scan().enumerate() {
            pos[i] = p;
        }
        let order = |v: &mut Vec<u32>| {
            if merge {
                v.sort_unstable();
            } else {
                v.sort_by_key(|&i| pos[i as usize]);
            }
        };
        let single_int = !keys.is_empty() && keys.iter().all(|(k, _)| k.len() == 1 && matches!(k[0], Value::Int8(_)));
        let mut cands = Vec::with_capacity(keys.len());
        let map = if single_int || keys.is_empty() {
            let mut m: super::fasthash::HashMap<i64, Vec<u32>> = Default::default();
            for (k, i) in keys {
                if let Value::Int8(x) = k[0] {
                    m.entry(x).or_default().push(i as u32);
                }
            }
            let mut out: super::fasthash::HashMap<i64, Span> = Default::default();
            out.reserve(m.len());
            for (k, mut v) in m {
                order(&mut v);
                out.insert(k, (cands.len() as u32, v.len() as u32));
                cands.extend(v);
            }
            KeyMap::Int(out)
        } else {
            let mut m: super::fasthash::HashMap<Box<[Value]>, Vec<u32>> = Default::default();
            for (k, i) in keys {
                m.entry(k.into_boxed_slice()).or_default().push(i as u32);
            }
            let mut out: super::fasthash::HashMap<Box<[Value]>, Span> = Default::default();
            out.reserve(m.len());
            for (k, mut v) in m {
                order(&mut v);
                out.insert(k, (cands.len() as u32, v.len() as u32));
                cands.extend(v);
            }
            KeyMap::Vals(out)
        };
        Built { rows, map, cands, table, cols: None }
    }

    /// The candidates of a normalised probe key (none for a NULL in it).
    pub fn lookup(&self, key: &[Value]) -> Span {
        if key.iter().any(Value::is_null) {
            return (0, 0);
        }
        match &self.map {
            // (Every inner key is an int8: only an integer can equal one.)
            KeyMap::Int(m) => match key {
                [v] => v.as_i64().and_then(|x| m.get(&x).copied()).unwrap_or((0, 0)),
                _ => (0, 0),
            },
            KeyMap::Vals(m) => m.get(key).copied().unwrap_or((0, 0)),
        }
    }

    /// The `i`th candidate of `span`.
    #[inline]
    pub fn cand(&self, span: Span, i: u32) -> usize {
        self.cands[(span.0 + i) as usize] as usize
    }

    /// Inner row `j`, as the join reads it.
    #[inline]
    pub fn inner(&self, j: usize) -> Inner<'_> {
        Inner { vals: &self.rows[j], map: self.cols.as_deref() }
    }

    /// The rows were built narrowed (`nodes::Narrow`): `places` is each inner column's
    /// place in them (`u32::MAX`: not kept, read as NULL).
    pub fn narrowed(&mut self, places: Box<[u32]>) {
        self.cols = Some(places);
    }

    /// Keep only the inner columns in `keep` (what the join's output, its condition and
    /// anything above read): the table holds each row's needed values and nothing for the
    /// others, which read as NULL (`Inner`). A row of 16 columns of which 3 are read took
    /// 16 values of 32 bytes.
    pub fn compact(&mut self, keep: &[bool]) {
        let width = self.rows.first().map_or(keep.len(), Vec::len);
        if self.cols.is_some() || keep.len() < width || keep.iter().take(width).all(|&k| k) {
            return;
        }
        let mut map = vec![u32::MAX; width];
        let mut n = 0;
        for (i, m) in map.iter_mut().enumerate() {
            if keep[i] {
                *m = n;
                n += 1;
            }
        }
        for r in &mut self.rows {
            let mut c = Vec::with_capacity(n as usize);
            c.extend(r.iter_mut().zip(keep).filter(|(_, k)| **k).map(|(v, _)| std::mem::replace(v, Value::Null)));
            *r = c;
        }
        self.cols = Some(map.into());
    }
}

/// An inner row of a hash table (`Built::inner`) or a plain row: column `i` of the inner
/// relation, NULL when the table didn't keep it.
#[derive(Clone, Copy)]
pub struct Inner<'a> {
    vals: &'a [Value],
    /// Each column's place in `vals` (`u32::MAX`: not kept); None: `vals` is the row.
    map: Option<&'a [u32]>,
}

static NULL: Value = Value::Null;

impl<'a> Inner<'a> {
    pub fn plain(vals: &'a [Value]) -> Inner<'a> {
        Inner { vals, map: None }
    }

    /// The inner relation's width.
    #[inline]
    pub fn width(&self) -> usize {
        self.map.map_or(self.vals.len(), <[u32]>::len)
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&'a Value> {
        match self.map {
            None => self.vals.get(i),
            Some(m) => match *m.get(i)? {
                u32::MAX => Some(&NULL),
                c => self.vals.get(c as usize),
            },
        }
    }

    /// The whole row (the columns not kept NULL).
    pub fn to_row(&self) -> Row {
        match self.map {
            None => self.vals.to_vec(),
            Some(_) => (0..self.width()).map(|i| self.get(i).cloned().unwrap_or(Value::Null)).collect(),
        }
    }
}

/// A row of `l`'s columns then `r`'s, copying only the columns in `keep` (None: all;
/// the others are NULL: nothing above reads them).
pub fn joined(l: &[Value], r: Inner, keep: Option<&[bool]>) -> Row {
    let mut out = Vec::with_capacity(l.len() + r.width());
    match (keep, r.map) {
        (None, None) => {
            out.extend_from_slice(l);
            out.extend_from_slice(r.vals);
        }
        (None, Some(_)) => {
            out.extend_from_slice(l);
            out.extend((0..r.width()).map(|i| r.get(i).cloned().unwrap_or(Value::Null)));
        }
        (Some(k), _) => {
            let kept = |i: usize| k.get(i).copied().unwrap_or(false);
            out.extend(l.iter().enumerate().map(|(i, v)| if kept(i) { v.clone() } else { Value::Null }));
            let n = l.len();
            out.extend((0..r.width()).map(|i| match r.get(i) {
                Some(v) if kept(n + i) => v.clone(),
                _ => Value::Null,
            }));
        }
    }
    out
}

/// One hash join's probe, as a worker runs it over the join's outer rows.
pub struct Probe {
    pub built: Arc<Built>,
    /// The outer keys and how each is normalised.
    pub keys: Vec<(K, Norm)>,
    /// The join condition beyond the keys, over the joined row.
    pub cond: Option<K>,
    /// The join has a condition no kernel does: an outer row with candidates goes to the
    /// executor's thread (its join node checks them), one without is the workers'.
    pub defer: bool,
    pub kind: JoinKind,
    pub nright: usize,
    /// The joined rows' columns anything reads (the condition's too; None: all).
    pub keep: Option<Box<[bool]>>,
    /// A right or full join's inner rows some outer row matched (its node puts out the
    /// others once the outer rows are done).
    pub matched: Option<Arc<[std::sync::atomic::AtomicBool]>>,
}

/// A joined row as a stage gives it: made (a row of its own), or its outer and inner rows
/// not copied yet (the next consumer may read it as it is: the partial aggregation).
pub enum Joined<'a> {
    Row(Row),
    Pair { l: &'a [Value], r: Inner<'a>, keep: Option<&'a [bool]> },
}

impl Joined<'_> {
    /// The joined row (`joined`'s copy of a pair).
    pub fn into_row(self) -> Row {
        match self {
            Joined::Row(r) => r,
            Joined::Pair { l, r, keep } => joined(l, r, keep),
        }
    }
}

impl super::kernel::Cols for Joined<'_> {
    #[inline]
    fn col(&self, i: usize) -> Option<&Value> {
        match self {
            Joined::Row(r) => r.get(i),
            Joined::Pair { l, r, .. } => {
                if i < l.len() {
                    l.get(i)
                } else {
                    r.get(i - l.len())
                }
            }
        }
    }
}

impl Probe {
    /// The inner rows outer row `l` joins (keys equal, the condition true), in the serial
    /// join's order, into `matches` (a semi or anti join: the first, if any). False: a
    /// kernel left something to `eval` (the row goes to the executor's thread).
    pub fn matches(&self, l: &[Value], key: &mut Vec<Value>, matches: &mut Vec<u32>) -> bool {
        key.clear();
        matches.clear();
        for (k, n) in &self.keys {
            match k.run(l) {
                Some(v) => key.push(norm(v, *n)),
                None => return false,
            }
        }
        let t = &*self.built;
        let span = t.lookup(key);
        if self.defer && span.1 > 0 {
            return false;
        }
        for i in 0..span.1 {
            let j = t.cand(span, i);
            if let Some(c) = &self.cond {
                let view = Joined::Pair { l, r: t.inner(j), keep: None };
                match c.passes(&view) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => return false,
                }
            }
            matches.push(j as u32);
            if matches!(self.kind, JoinKind::Semi | JoinKind::Anti) {
                break;
            }
        }
        if let Some(m) = &self.matched {
            for &j in matches.iter() {
                m[j as usize].store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        true
    }

    /// The join's rows for outer row `l` and its `matches`, in order, to `f`.
    pub fn emit(&self, l: Row, matches: &[u32], f: &mut dyn FnMut(Joined)) {
        let t = &*self.built;
        match self.kind {
            JoinKind::Semi if !matches.is_empty() => f(Joined::Row(l)),
            JoinKind::Anti if matches.is_empty() => f(Joined::Row(l)),
            JoinKind::Semi | JoinKind::Anti => {}
            JoinKind::Left | JoinKind::Full if matches.is_empty() => {
                let mut o = l;
                o.extend(std::iter::repeat_n(Value::Null, self.nright));
                f(Joined::Row(o));
            }
            _ => {
                for &j in matches {
                    f(Joined::Pair { l: &l, r: t.inner(j as usize), keep: self.keep.as_deref() });
                }
            }
        }
    }
}

/// A pipeline's stage, as the workers run it over the rows of the stage below: a hash
/// join's probe, or a nested loop's inner index scans (`lookup`).
pub enum Stage {
    Hash(Probe),
    Lookup(Box<super::lookup::Lookup>),
}

/// What a pipeline's node gives the node above: a row through `.1` stages (the scan's
/// own rows are 0; `ALL`: a node outside a pipeline's), or an aggregated part.
pub enum Item {
    Row(Row, u8),
    Part(Part),
}

/// The level of a row no stage is left to run on.
pub const ALL: u8 = u8::MAX;

/// Scratch for `run_stages`: per stage, its matches (a lookup's: its rows) and the key
/// vector.
#[derive(Default)]
pub struct Scratch {
    matches: Vec<Vec<u32>>,
    rows: Vec<Vec<Row>>,
    key: Vec<Value>,
    lookups: Vec<super::lookup::LookupScratch>,
}

impl Scratch {
    /// The lookup stages' counts so far, into the stages (as a chunk ends).
    pub fn flush(&mut self, stages: &[Arc<Stage>]) {
        for (st, sc) in stages.iter().zip(self.lookups.iter_mut()) {
            if let Stage::Lookup(l) = &**st {
                l.take_counts(sc);
            }
        }
    }
}

/// `row` (through stages `..from`) through `stages[from..]`; each row that comes out goes
/// to `emit` with the stages it went through (all of them, or fewer where a kernel left
/// one to the executor's thread). The last stage's rows come as they are (`Joined`).
pub fn run_stages(stages: &[Arc<Stage>], from: usize, row: Row, s: &mut Scratch, emit: &mut dyn FnMut(Joined, u8)) {
    if from == stages.len() {
        emit(Joined::Row(row), from as u8);
        return;
    }
    if s.matches.len() < stages.len() {
        s.matches.resize_with(stages.len(), Vec::new);
        s.rows.resize_with(stages.len(), Vec::new);
        s.lookups.resize_with(stages.len(), Default::default);
    }
    match &*stages[from] {
        Stage::Hash(p) => {
            let mut matches = std::mem::take(&mut s.matches[from]);
            match p.matches(&row, &mut s.key, &mut matches) {
                false => emit(Joined::Row(row), from as u8),
                true => p.emit(row, &matches, &mut |j| onward(stages, from, j, s, emit)),
            }
            s.matches[from] = matches;
        }
        Stage::Lookup(lk) => {
            let mut rows = std::mem::take(&mut s.rows[from]);
            match lk.rows(&row, &mut rows, &mut s.lookups[from]) {
                false => emit(Joined::Row(row), from as u8),
                true => lk.emit(row, &rows, &mut |j| onward(stages, from, j, s, emit)),
            }
            s.rows[from] = rows;
        }
    }
}

/// A row stage `from` made, to the stage after it (or out, after the last).
fn onward(stages: &[Arc<Stage>], from: usize, j: Joined, s: &mut Scratch, emit: &mut dyn FnMut(Joined, u8)) {
    if from + 1 == stages.len() {
        emit(j, from as u8 + 1);
    } else {
        run_stages(stages, from + 1, j.into_row(), s, emit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::numeric::Numeric;
    use std::hash::BuildHasher;

    fn num(i: i64) -> Value {
        Value::Numeric(Arc::new(Numeric::from_i64(i)))
    }

    /// Equal key values of the two sides hash alike once normalised for their types'
    /// pair; without that, an int8 and a numeric that `Value` finds equal hash apart, and
    /// a hash table would match them only when their hashes happened to collide.
    #[test]
    fn keys_of_both_sides_hash_alike() {
        let st = super::super::fasthash::FastState::default();
        assert_eq!(Value::Int8(5), num(5));
        assert_ne!(st.hash_one(Value::Int8(5)), st.hash_one(num(5)), "the hazard");
        let (i8t, nt, f8t, f4t, i4t) =
            (DataType::Int8, DataType::NUMERIC, DataType::Float8, DataType::Float4, DataType::Int4);
        let cases: Vec<(Value, &DataType, Value, &DataType)> = vec![
            (Value::Int8(5), &i8t, num(5), &nt),
            (Value::Int4(5), &i4t, Value::Int8(5), &i8t),
            (Value::Float4(0.5), &f4t, Value::Float8(0.5), &f8t),
            (Value::Int4(3), &i4t, Value::Float8(3.0), &f8t),
            (num(7), &nt, Value::Float8(7.0), &f8t),
        ];
        for (a, at, b, bt) in cases {
            let (na, nb) = (norm(a.clone(), norm_for(at, bt)), norm(b.clone(), norm_for(bt, at)));
            assert_eq!(na, nb, "{a:?} {b:?}");
            assert_eq!(st.hash_one(&na), st.hash_one(&nb), "{a:?} {b:?}");
        }
        // bpchar compares without its trailing spaces.
        assert_eq!(norm(Value::text("ab  "), Norm::Bpchar), Value::text("ab"));
    }

    /// A table of one integer key finds every integer probe equal to a key, nothing else.
    #[test]
    fn single_integer_keys() {
        let rows: Vec<Row> = (0..6).map(|i| vec![Value::Int4(i % 3)]).collect();
        let keys: Vec<(Vec<Value>, usize)> = (0..6).map(|i| (vec![Value::Int8(i as i64 % 3)], i)).collect();
        let table = super::super::pghash::JoinTable::build(1024, (0..6).map(|i| (i, i as u32 % 3, 0)).collect());
        let b = Built::new(rows, keys, table, true);
        let span = b.lookup(&[Value::Int8(1)]);
        assert_eq!((0..span.1).map(|i| b.cand(span, i)).collect::<Vec<_>>(), vec![1, 4]);
        assert_eq!(b.lookup(&[Value::Oid(2)]).1, 2);
        assert_eq!(b.lookup(&[Value::Int8(7)]).1, 0);
        assert_eq!(b.lookup(&[Value::Null]).1, 0);
        assert_eq!(b.lookup(&[num(1)]).1, 0);
    }
}
