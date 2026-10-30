//! Search structures for the index access methods whose scans don't read their stored
//! entries (GiST, SP-GiST, BRIN, GIN: `scan::reads_entries`): their scans and exclusion
//! checks read the rows the structure places near their
//! conditions, rather than every row of the table. A structure is derived from the table:
//! built from its rows as of a time and kept up with every commit since through
//! `txn::watch` (and the transaction's own writes, read from its buffer each time). It only
//! narrows: every candidate's row is read and its conditions checked as before, so a
//! structure that places a row loosely (bounding boxes, inclusive bounds, a NaN) costs a
//! read, never a result.
//!
//! What it keys: one range, geometric or ordered scalar column (an interval per row, two
//! for a box), and the columns of integer-like types compared by `=` (a partition each).
//! Other conditions narrow nothing; an index with nothing to key, a scan whose snapshot is
//! older than the structure, or a serializable scan (its predicate locks are the full
//! scan's) reads every row.

use super::ExecCtx;
use super::dml::index_values;
use super::scan::decode_table_row;
use crate::catalog::Table;
use crate::error::Result;
use crate::sql::plan::BoundIndex;
use crate::storage::keys::{encode_index_value, index_prefix, prefix_end};
use crate::txn::watch::State;
use crate::types::geo::Geo;
use crate::types::{DataType, Value};
use std::collections::HashMap;
use std::sync::Arc;

mod gin;

/// Rows' data keys (past the table's prefix), back to back: row `i` of a structure is its
/// `i`th key (4 bytes and the key's own per row, no allocation each).
#[derive(Default)]
struct RowKeys {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl RowKeys {
    fn push(&mut self, k: &[u8]) -> u32 {
        self.bytes.extend_from_slice(k);
        self.ends.push(self.bytes.len() as u32);
        self.ends.len() as u32 - 1
    }
    fn len(&self) -> usize {
        self.ends.len()
    }
    fn get(&self, i: u32) -> &[u8] {
        let i = i as usize;
        let a = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[a..self.ends[i] as usize]
    }
    /// Row `i`'s data key, after `prefix` (the table's).
    fn key(&self, prefix: &[u8], i: u32) -> Vec<u8> {
        let s = self.get(i);
        let mut k = Vec::with_capacity(prefix.len() + s.len());
        k.extend_from_slice(prefix);
        k.extend_from_slice(s);
        k
    }
    /// Whether another key could overflow its offsets.
    fn full(&self) -> bool {
        self.bytes.len() > (u32::MAX as usize) - (1 << 20)
    }
    fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + 4 * self.ends.capacity()
    }
    fn shrink(&mut self) {
        self.bytes.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
}

/// A posting list: ascending row numbers, each stored as its distance from the one before
/// (7 bits a byte, the high bit set on every byte but a number's last), as GIN compresses
/// its posting lists: a byte or two a row where a list holds many.
#[derive(Default)]
struct Posting {
    bytes: Vec<u8>,
    last: u32,
    len: u32,
}

impl Posting {
    /// Appends `row` (greater than every row in it).
    fn push(&mut self, row: u32) {
        let mut d = if self.len == 0 { row } else { row - self.last };
        while d >= 0x80 {
            self.bytes.push((d as u8) | 0x80);
            d >>= 7;
        }
        self.bytes.push(d as u8);
        self.last = row;
        self.len += 1;
    }
    fn len(&self) -> usize {
        self.len as usize
    }
    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        let mut pos = 0;
        let mut at = 0u32;
        std::iter::from_fn(move || {
            let mut d = 0u32;
            let mut shift = 0;
            loop {
                let b = *self.bytes.get(pos)?;
                pos += 1;
                d |= ((b & 0x7F) as u32) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            at += d;
            Some(at)
        })
    }
    fn heap_bytes(&self) -> usize {
        self.bytes.capacity()
    }
}

/// The rows of `a` (ascending) also in `b`.
fn intersect_posting(a: &[u32], b: &Posting) -> Vec<u32> {
    let mut out = Vec::new();
    let mut it = b.iter().peekable();
    for &r in a {
        while it.next_if(|x| *x < r).is_some() {}
        match it.peek() {
            Some(x) if *x == r => out.push(r),
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Range,
    Geo,
    Scalar,
}

/// The column the structure orders rows by, and the `=` columns partitioning them (with
/// every key column's type).
#[derive(Clone, Debug, PartialEq)]
struct Shape {
    dim: Option<(usize, Kind)>,
    parts: Vec<usize>,
    tys: Vec<DataType>,
}

fn ordered_scalar(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Date | DataType::Timestamp(_) | DataType::Timestamptz(_)
            | DataType::Float4 | DataType::Float8
    )
}

/// Whether `v` keys as a value of a column of type `ty` does (`ord_key`'s scale, or
/// `encode_index_value`'s bytes for `=`): an argument of a cross-type operator may not.
fn fits(ty: &DataType, v: &Value) -> bool {
    matches!(
        (ty, v),
        (DataType::Int2 | DataType::Int4 | DataType::Int8, Value::Int2(_) | Value::Int4(_) | Value::Int8(_))
            | (DataType::Float4 | DataType::Float8, Value::Float4(_) | Value::Float8(_))
            | (DataType::Date, Value::Date(_))
            | (DataType::Timestamp(_), Value::Timestamp(_))
            | (DataType::Timestamptz(_), Value::Timestamptz(_))
            | (DataType::Bool, Value::Bool(_))
            | (DataType::Uuid, Value::Uuid(_))
            | (DataType::Oid, Value::Oid(_))
    )
}

fn shape(bix: &BoundIndex) -> Option<Shape> {
    let mut dim = None;
    let mut parts = Vec::new();
    let tys: Vec<DataType> = bix.keys.iter().map(|k| k.ty()).collect();
    for (i, ty) in tys.iter().enumerate() {
        let kind = match ty {
            DataType::Range(rt) if matches!(rt.oid, 3904 | 3926 | 3908 | 3910 | 3912) => Some(Kind::Range),
            DataType::Point | DataType::GBox | DataType::Polygon | DataType::Circle => Some(Kind::Geo),
            _ => None,
        };
        match kind {
            Some(k) if dim.is_none_or(|(_, d)| d == Kind::Scalar) => dim = Some((i, k)),
            Some(_) => {}
            None => {
                // (Byte-equal exactly when `=`: no collations, no -0, no numeric scales.)
                if matches!(ty, DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Bool | DataType::Date | DataType::Uuid | DataType::Oid | DataType::Timestamp(_) | DataType::Timestamptz(_)) {
                    parts.push(i);
                }
                if dim.is_none() && ordered_scalar(ty) {
                    dim = Some((i, Kind::Scalar));
                }
            }
        }
    }
    if let Some((i, Kind::Scalar)) = dim {
        parts.retain(|&p| p != i);
    }
    (dim.is_some() || !parts.is_empty()).then_some(Shape { dim, parts, tys })
}

/// An order-preserving key of an ordered value (None: none, or NaN).
fn ord_key(v: &Value) -> Option<u64> {
    let i = |x: i64| (x as u64) ^ (1 << 63);
    let f = |x: f64| {
        if x.is_nan() {
            return None;
        }
        let b = if x == 0.0 { 0.0f64 } else { x }.to_bits();
        Some(if b >> 63 == 1 { !b } else { b | (1 << 63) })
    };
    match v {
        Value::Int2(x) => Some(i(*x as i64)),
        Value::Int4(x) | Value::Date(x) => Some(i(*x as i64)),
        Value::Int8(x) | Value::Timestamp(x) | Value::Timestamptz(x) => Some(i(*x)),
        Value::Float4(x) => f(*x as f64),
        Value::Float8(x) => f(*x),
        _ => None,
    }
}

/// A geometric value's bounding box ((lx, hx), (ly, hy)), widened a little past
/// PostgreSQL's fuzzy comparisons (EPSILON) when `fuzz`.
fn geo_box(v: &Value, fuzz: bool) -> Option<((u64, u64), (u64, u64))> {
    let Value::Geo(g) = v else { return None };
    let (lx, ly, hx, hy) = match &**g {
        Geo::Point(p) => (p.x, p.y, p.x, p.y),
        Geo::Box(b) => (b.low.x, b.low.y, b.high.x, b.high.y),
        Geo::Polygon(p) => (p.boundbox.low.x, p.boundbox.low.y, p.boundbox.high.x, p.boundbox.high.y),
        Geo::Circle(c) => (c.center.x - c.radius, c.center.y - c.radius, c.center.x + c.radius, c.center.y + c.radius),
        _ => return None,
    };
    let w = |x: f64, d: f64| if fuzz { x + d * (1e-5 + x.abs() * 1e-9) } else { x };
    let k = |x: f64| ord_key(&Value::Float8(x));
    Some(((k(w(lx, -1.0))?, k(w(hx, 1.0))?), (k(w(ly, -1.0))?, k(w(hy, 1.0))?)))
}

/// A range's bounds as keys (unbounded: the ends of the key space); None: empty.
fn range_keys(v: &Value) -> Option<Option<(u64, u64)>> {
    let Value::Range(r) = v else { return None };
    if r.empty {
        return Some(None);
    }
    let lo = match &r.lower {
        None => 0,
        Some(b) => ord_key(b)?,
    };
    let hi = match &r.upper {
        None => u64::MAX,
        Some(b) => ord_key(b)?,
    };
    Some(Some((lo, hi)))
}

/// Where a row goes: its partition, its intervals; or always a candidate (None), or an
/// empty range.
enum Place {
    At(Vec<u8>, (u64, u64), (u64, u64)),
    Empty(Vec<u8>),
    Always,
}

fn place(sh: &Shape, vals: &[Value]) -> Place {
    let mut part = Vec::new();
    for &p in &sh.parts {
        encode_index_value(&mut part, &vals[p], false, false, None);
    }
    let all = (0, u64::MAX);
    let Some((i, kind)) = sh.dim else {
        return Place::At(part, all, all);
    };
    let v = &vals[i];
    if v.is_null() {
        return Place::Always;
    }
    match kind {
        Kind::Range => match range_keys(v) {
            Some(Some(x)) => Place::At(part, x, all),
            Some(None) => Place::Empty(part),
            None => Place::Always,
        },
        Kind::Geo => match geo_box(v, false) {
            Some((x, y)) => Place::At(part, x, y),
            None => Place::Always,
        },
        Kind::Scalar => match ord_key(v) {
            Some(k) => Place::At(part, (k, k), all),
            None => Place::Always,
        },
    }
}

/// Bounds on a row's interval in one dimension: its low end in `lo`, its high end in `hi`
/// (inclusive).
#[derive(Clone, Copy, Debug)]
struct Span {
    lo: (u64, u64),
    hi: (u64, u64),
}

const ANY: Span = Span { lo: (0, u64::MAX), hi: (0, u64::MAX) };

impl Span {
    fn lo_max(&mut self, v: u64) {
        self.lo.1 = self.lo.1.min(v);
    }
    fn lo_min(&mut self, v: u64) {
        self.lo.0 = self.lo.0.max(v);
    }
    fn hi_max(&mut self, v: u64) {
        self.hi.1 = self.hi.1.min(v);
    }
    fn hi_min(&mut self, v: u64) {
        self.hi.0 = self.hi.0.max(v);
    }
}

/// The rows a scan's conditions may hold for.
struct Query {
    /// The partition (None: every one).
    part: Option<Vec<u8>>,
    x: Span,
    y: Span,
    /// Whether empty ranges may match.
    empties: bool,
    /// Whether any condition narrowed it.
    narrowed: bool,
}

/// Narrows `q` by `op(row value, arg)` on the ordered column, of kind `kind`. (Bounds are
/// inclusive and geometric ones widened: each test holds for at least the rows the
/// operator holds for.)
fn narrow(q: &mut Query, kind: Kind, op: &str, arg: &Value) {
    match kind {
        Kind::Range => {
            // An element (`@>` a value) is the range [v, v].
            let a = match arg {
                Value::Range(_) => match range_keys(arg) {
                    Some(Some(a)) => a,
                    // (Empty, or bounds without keys: nothing narrows.)
                    _ => return,
                },
                v => match ord_key(v) {
                    Some(k) => (k, k),
                    None => return,
                },
            };
            match op {
                "&&" | "-|-" => {
                    q.x.lo_max(a.1);
                    q.x.hi_min(a.0);
                    q.empties = false;
                }
                "@>" => {
                    q.x.lo_max(a.0);
                    q.x.hi_min(a.1);
                    q.empties = false;
                }
                "<@" => {
                    q.x.lo_min(a.0);
                    q.x.hi_max(a.1);
                }
                "=" => {
                    q.x.lo_min(a.0);
                    q.x.lo_max(a.0);
                    q.x.hi_min(a.1);
                    q.x.hi_max(a.1);
                    q.empties = false;
                }
                "<<" | "&<" => {
                    q.x.hi_max(if op == "<<" { a.0 } else { a.1 });
                    q.empties = false;
                }
                ">>" | "&>" => {
                    q.x.lo_min(if op == ">>" { a.1 } else { a.0 });
                    q.empties = false;
                }
                _ => return,
            }
        }
        Kind::Geo => {
            let Some((ax, ay)) = geo_box(arg, true) else { return };
            match op {
                "&&" | "@>" | "~" => {
                    q.x.lo_max(ax.1);
                    q.x.hi_min(ax.0);
                    q.y.lo_max(ay.1);
                    q.y.hi_min(ay.0);
                }
                "<@" | "@" | "~=" => {
                    q.x.lo_min(ax.0);
                    q.x.hi_max(ax.1);
                    q.y.lo_min(ay.0);
                    q.y.hi_max(ay.1);
                }
                "<<" | "&<" => q.x.hi_max(ax.1),
                ">>" | "&>" => q.x.lo_min(ax.0),
                "<<|" | "<^" | "&<|" => q.y.hi_max(ay.1),
                "|>>" | ">^" | "|&>" => q.y.lo_min(ay.0),
                _ => return,
            }
        }
        Kind::Scalar => {
            let Some(k) = ord_key(arg) else { return };
            match op {
                "=" => {
                    q.x.lo_min(k);
                    q.x.lo_max(k);
                }
                "<" | "<=" => q.x.lo_max(k),
                ">" | ">=" => q.x.lo_min(k),
                _ => return,
            }
        }
    }
    q.narrowed = true;
}

/// The query of conditions `conds` (index column, operator, argument) on an index of
/// shape `sh`.
fn query(sh: &Shape, conds: &[(usize, &str, Value)]) -> Query {
    let mut q = Query { part: None, x: ANY, y: ANY, empties: true, narrowed: false };
    let mut part = Vec::new();
    for &p in &sh.parts {
        match conds.iter().find(|(c, op, v)| *c == p && *op == "=" && fits(&sh.tys[p], v)) {
            Some((_, _, v)) => encode_index_value(&mut part, v, false, false, None),
            None => {
                part.clear();
                break;
            }
        }
    }
    if !sh.parts.is_empty() && !part.is_empty() {
        q.part = Some(part);
        q.narrowed = true;
    }
    if let Some((i, kind)) = sh.dim {
        for (c, op, v) in conds {
            // (A range's or a geometric column's argument: keyed by its own type.)
            if *c == i && !v.is_null() && (kind != Kind::Scalar || fits(&sh.tys[i], v)) {
                narrow(&mut q, kind, op, v);
            }
        }
    }
    q
}

#[derive(Clone, Copy)]
struct Ent {
    part: u32,
    lo: u32,
    hi: u32,
    row: u32,
}

/// A monotone map of 64-bit keys onto 32 bits (`Grid`'s entries): keys from `base` on,
/// `shift` bits down, clamped at both ends. A test on mapped keys holds wherever it holds on
/// the keys, so mapped entries place rows a little more loosely, never wrongly.
#[derive(Clone, Copy, Debug)]
struct Quant {
    base: u64,
    shift: u32,
}

impl Quant {
    /// The top 32 bits.
    const TOP: Quant = Quant { base: 0, shift: 32 };

    fn q(self, x: u64) -> u32 {
        (x.saturating_sub(self.base) >> self.shift).min(u32::MAX as u64) as u32
    }

    /// One that keeps most of `keys` apart: from their 0.1th to their 99.9th percentile
    /// (unbounded ends left out).
    fn fit(mut keys: Vec<u64>) -> Quant {
        keys.retain(|&k| k != 0 && k != u64::MAX);
        if keys.is_empty() {
            return Quant::TOP;
        }
        let n = keys.len();
        let lo = *keys.select_nth_unstable(n / 1000).1;
        let hi = *keys.select_nth_unstable(n - 1 - n / 1000).1;
        Quant { base: lo, shift: (64 - (hi - lo).leading_zeros()).saturating_sub(32) }
    }

    fn span(self, s: &Span) -> QSpan {
        QSpan { lo: (self.q(s.lo.0), self.q(s.lo.1)), hi: (self.q(s.hi.0), self.q(s.hi.1)) }
    }
}

/// A `Span` mapped by a `Quant`.
#[derive(Clone, Copy)]
struct QSpan {
    lo: (u32, u32),
    hi: (u32, u32),
}

impl QSpan {
    fn holds(&self, (lo, hi): (u32, u32)) -> bool {
        self.lo.0 <= lo && lo <= self.lo.1 && self.hi.0 <= hi && hi <= self.hi.1
    }
}

/// A row placed while a `Grid` is built, before its keys are mapped: partition, intervals.
type RawEnt = (u32, (u64, u64), (u64, u64), u32);

/// Entries per block of `Grid::sorted`, whose high ends' range skips it.
const BLOCK: usize = 64;

/// The rows of a table placed for an index of shape `shape`.
struct Grid {
    shape: Shape,
    /// What the index's definition was (a changed one is a new structure).
    fingerprint: String,
    /// The time its rows were read at: older snapshots can't use it.
    ts: u64,
    parts: HashMap<Vec<u8>, u32>,
    /// Each entry's row: its data key past the table's prefix, and its second interval
    /// (a box's y; two-dimensional shapes only).
    rows: RowKeys,
    ys: Vec<(u32, u32)>,
    /// How its keys are mapped, per dimension; the rows placed as it is built, mapped
    /// once their keys' spread is known.
    qx: Quant,
    qy: Quant,
    raw: Option<Vec<RawEnt>>,
    /// Entries in (partition, low end) order, the high ends' (min, max) per block, and the
    /// entries added since they were last sorted.
    sorted: Vec<Ent>,
    blocks: Vec<(u32, u32)>,
    delta: Vec<Ent>,
    /// Rows always read (values it can't place) and empty ranges (by partition).
    always: Vec<u32>,
    empties: Vec<(u32, u32)>,
    /// Rows when it was built: past twice that (rows written again, deleted) it is rebuilt.
    built: usize,
}

impl Grid {
    fn new(shape: Shape, fingerprint: String, ts: u64) -> Grid {
        Grid {
            shape,
            fingerprint,
            ts,
            parts: HashMap::new(),
            rows: RowKeys::default(),
            ys: Vec::new(),
            qx: Quant::TOP,
            qy: Quant::TOP,
            raw: None,
            sorted: Vec::new(),
            blocks: Vec::new(),
            delta: Vec::new(),
            always: Vec::new(),
            empties: Vec::new(),
            built: 0,
        }
    }

    fn two_d(&self) -> bool {
        matches!(self.shape.dim, Some((_, Kind::Geo)))
    }

    fn part_id(&mut self, p: Vec<u8>) -> u32 {
        let n = self.parts.len() as u32;
        *self.parts.entry(p).or_insert(n)
    }

    /// Adds row `suffix` (its data key past the table's prefix) with index values `vals`
    /// (None: they couldn't be computed).
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        let row = self.rows.push(suffix);
        let place = vals.map_or(Place::Always, |v| place(&self.shape, v));
        let mut y = (0, u64::MAX);
        match place {
            Place::Always => self.always.push(row),
            Place::Empty(p) => {
                let p = self.part_id(p);
                self.empties.push((p, row));
            }
            Place::At(p, x, yy) => {
                let part = self.part_id(p);
                y = yy;
                match &mut self.raw {
                    Some(raw) => raw.push((part, x, yy, row)),
                    None => self.delta.push(Ent { part, lo: self.qx.q(x.0), hi: self.qx.q(x.1), row }),
                }
            }
        }
        if self.raw.is_some() {
            return;
        }
        if self.two_d() {
            self.ys.push((self.qy.q(y.0), self.qy.q(y.1)));
        }
        if self.delta.len() > 1024.max(self.sorted.len() / 8) {
            self.merge();
        }
    }

    /// The rows placed as it was built, mapped by quantizers fit to them.
    fn map_raw(&mut self) {
        let Some(raw) = self.raw.take() else { return };
        self.qx = Quant::fit(raw.iter().flat_map(|e| [e.1.0, e.1.1]).collect());
        if self.two_d() {
            self.qy = Quant::fit(raw.iter().flat_map(|e| [e.2.0, e.2.1]).collect());
            self.ys = vec![(self.qy.q(0), self.qy.q(u64::MAX)); self.rows.len()];
        }
        self.delta.reserve(raw.len());
        for (part, x, y, row) in raw {
            self.delta.push(Ent { part, lo: self.qx.q(x.0), hi: self.qx.q(x.1), row });
            if self.two_d() {
                self.ys[row as usize] = (self.qy.q(y.0), self.qy.q(y.1));
            }
        }
    }

    fn merge(&mut self) {
        self.sorted.append(&mut self.delta);
        self.sorted.sort_unstable_by_key(|e| (e.part, e.lo, e.row));
        self.blocks = self
            .sorted
            .chunks(BLOCK)
            .map(|c| c.iter().fold((u32::MAX, 0), |(a, b), e| (a.min(e.hi), b.max(e.hi))))
            .collect();
    }

    fn ent_ok(&self, x: &QSpan, y: &QSpan, e: &Ent) -> bool {
        x.holds((e.lo, e.hi)) && (!self.two_d() || y.holds(self.ys[e.row as usize]))
    }

    /// The rows `q` may hold for.
    fn search(&self, q: &Query, out: &mut Vec<u32>) {
        let part = match &q.part {
            Some(p) => match self.parts.get(p) {
                Some(&id) => Some(id),
                // (No row of that partition.)
                None => {
                    out.extend(&self.always);
                    return;
                }
            },
            None => None,
        };
        let parts: Vec<u32> = match part {
            Some(p) => vec![p],
            None => (0..self.parts.len() as u32).collect(),
        };
        let (x, y) = (self.qx.span(&q.x), self.qy.span(&q.y));
        for p in parts {
            let s = &self.sorted;
            let a = s.partition_point(|e| (e.part, e.lo) < (p, x.lo.0));
            let b = s.partition_point(|e| (e.part, e.lo) <= (p, x.lo.1));
            let mut i = a;
            while i < b {
                let blk = i / BLOCK;
                let end = ((blk + 1) * BLOCK).min(b);
                let (mn, mx) = self.blocks[blk];
                if mx >= x.hi.0 && mn <= x.hi.1 {
                    out.extend(s[i..end].iter().filter(|e| self.ent_ok(&x, &y, e)).map(|e| e.row));
                }
                i = end;
            }
            if q.empties {
                out.extend(self.empties.iter().filter(|(ep, _)| *ep == p).map(|(_, r)| *r));
            }
        }
        out.extend(self.delta.iter().filter(|e| part.is_none_or(|p| e.part == p) && self.ent_ok(&x, &y, e)).map(|e| e.row));
        out.extend(&self.always);
    }
}

/// Rows per zone of `Zones`.
const ZONE: u32 = 128;

/// A BRIN index's structure: the table's rows in data key order cut into zones of about
/// `ZONE` rows, each with the bounds of its rows' intervals (and whether it has an empty
/// range, or a row it can't place). Rows are read by key range, as BRIN reads block ranges:
/// it works as well as the column follows the rows' order, and holds a few bytes per zone.
struct Zones {
    shape: Shape,
    fingerprint: String,
    ts: u64,
    zones: Vec<Zone>,
    rows: usize,
    built: usize,
}

struct Zone {
    first: Box<[u8]>,
    last: Box<[u8]>,
    n: u32,
    /// (min, max) of the rows' low ends and of their high ends, per dimension.
    x: [(u64, u64); 2],
    y: [(u64, u64); 2],
    empties: bool,
    always: bool,
}

const NONE: [(u64, u64); 2] = [(u64::MAX, 0), (u64::MAX, 0)];

fn widen(b: &mut [(u64, u64); 2], (lo, hi): (u64, u64)) {
    b[0] = (b[0].0.min(lo), b[0].1.max(lo));
    b[1] = (b[1].0.min(hi), b[1].1.max(hi));
}

fn meets(b: &[(u64, u64); 2], s: &Span) -> bool {
    b[0].0 <= s.lo.1 && b[0].1 >= s.lo.0 && b[1].0 <= s.hi.1 && b[1].1 >= s.hi.0
}

impl Zones {
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        self.rows += 1;
        let at = self.zones.partition_point(|z| &*z.first <= suffix);
        let fresh = || Zone { first: suffix.into(), last: suffix.into(), n: 0, x: NONE, y: NONE, empties: false, always: false };
        let i = match at {
            // (Before the first zone: it starts earlier.)
            0 => {
                if self.zones.is_empty() {
                    self.zones.push(fresh());
                } else {
                    self.zones[0].first = suffix.into();
                }
                0
            }
            a if a == self.zones.len() && &*self.zones[a - 1].last < suffix && self.zones[a - 1].n >= ZONE => {
                self.zones.push(fresh());
                a
            }
            a => {
                let z = &mut self.zones[a - 1];
                if &*z.last < suffix {
                    z.last = suffix.into();
                }
                a - 1
            }
        };
        let z = &mut self.zones[i];
        z.n += 1;
        match vals.map_or(Place::Always, |v| place(&self.shape, v)) {
            Place::Always => z.always = true,
            Place::Empty(_) => z.empties = true,
            Place::At(_, x, y) => {
                widen(&mut z.x, x);
                widen(&mut z.y, y);
            }
        }
    }

    /// The key ranges (past the table's prefix; inclusive) of the zones `q` may hold in.
    fn search(&self, q: &Query) -> Vec<(&[u8], &[u8])> {
        let mut out: Vec<(&[u8], &[u8])> = Vec::new();
        let mut prev = false;
        for z in &self.zones {
            let hit = z.always || (q.empties && z.empties) || (z.x[0].0 <= z.x[0].1 && meets(&z.x, &q.x) && meets(&z.y, &q.y));
            if hit {
                // (Next to the last one taken: one range.)
                match out.last_mut() {
                    Some(last) if prev => last.1 = &z.last,
                    _ => out.push((&z.first, &z.last)),
                }
            }
            prev = hit;
        }
        out
    }
}

/// The memory one structure may hold: half the block cache, at least 128 MB (a few million
/// rows: 30-60 bytes each placed one by one, a byte or two per trigram or element). Past
/// it, its table's scans read every row.
fn budget(ctx: &ExecCtx) -> usize {
    (ctx.sess.db.store.cache_capacity() / 2).max(128 << 20)
}

/// An index whose structure would hold more than `budget` (its definition's fingerprint).
struct TooBig(String);

/// What `structure` keeps for an index: rows placed one by one (`Grid`), or by zones
/// (`Zones`, BRIN).
trait Placing: Send + Sized + 'static {
    /// Whether it holds every row (and is kept within `budget`).
    const ROW_CAP: bool;
    fn make(shape: Shape, fingerprint: String, ts: u64) -> Self;
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>);
    fn built(&mut self);
    fn fingerprint(&self) -> &str;
    /// Rows added again (written again, deleted) have piled up: built afresh.
    fn stale(&self) -> bool;
    /// The memory it holds (bytes).
    fn heap_bytes(&self) -> usize;
    /// Whether its row keys are as many bytes as it can address.
    fn full(&self) -> bool {
        false
    }
}

impl Placing for Grid {
    const ROW_CAP: bool = true;
    fn make(shape: Shape, fingerprint: String, ts: u64) -> Grid {
        Grid { raw: Some(Vec::new()), ..Grid::new(shape, fingerprint, ts) }
    }
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        Grid::add(self, suffix, vals);
    }
    fn built(&mut self) {
        self.map_raw();
        self.merge();
        self.built = self.rows.len();
        self.rows.shrink();
        self.sorted.shrink_to_fit();
        self.delta = Vec::new();
        self.ys.shrink_to_fit();
    }
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    fn stale(&self) -> bool {
        self.rows.len() > 2 * self.built + 4096
    }
    fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        self.rows.heap_bytes()
            + self.parts.keys().map(|k| k.capacity() + size_of::<(Vec<u8>, u32)>() + 1).sum::<usize>()
            + self.ys.capacity() * size_of::<(u32, u32)>()
            + self.raw.as_ref().map_or(0, |r| r.capacity() * size_of::<RawEnt>())
            + (self.sorted.capacity() + self.delta.capacity()) * size_of::<Ent>()
            + self.blocks.capacity() * size_of::<(u32, u32)>()
            + self.always.capacity() * 4
            + self.empties.capacity() * 8
    }
    fn full(&self) -> bool {
        self.rows.full()
    }
}

impl Placing for Zones {
    const ROW_CAP: bool = false;
    fn make(shape: Shape, fingerprint: String, ts: u64) -> Zones {
        Zones { shape, fingerprint, ts, zones: Vec::new(), rows: 0, built: 0 }
    }
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        Zones::add(self, suffix, vals);
    }
    fn built(&mut self) {
        self.built = self.rows;
    }
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    fn stale(&self) -> bool {
        self.rows > 2 * self.built + 4096
    }
    fn heap_bytes(&self) -> usize {
        self.zones.capacity() * std::mem::size_of::<Zone>() + self.zones.iter().map(|z| z.first.len() + z.last.len()).sum::<usize>()
    }
}

fn fingerprint(bix: &BoundIndex) -> String {
    format!("{:?}|{:?}", bix.keys, bix.index.columns)
}

/// `bix` without its predicate: a partial index's structure places every row (a row the
/// predicate leaves out is read as every row was before).
fn unpredicated(bix: &BoundIndex) -> BoundIndex {
    BoundIndex { predicate: None, ..bix.clone() }
}

/// A committed row as the log has it, with its out-of-line values put back
/// (`storage::toast`: they are written once under keys of their own, so the logged row's
/// pointers still name its values). None: one is gone already (the row is then read by
/// every scan, as a row it can't place).
fn spliced(db: &crate::txn::Database, key: &[u8], v: bytes::Bytes) -> Option<bytes::Bytes> {
    if !crate::storage::toast::has_external(&v) {
        return Some(v);
    }
    let mut fetch = |k: &[u8]| db.store.get(k, u64::MAX)?.ok_or_else(|| crate::error::internal("an out-of-line value is missing"));
    crate::storage::toast::splice(key, &v, None, &mut fetch).ok().map(bytes::Bytes::from)
}

/// The values a stored row has for the index (None: its row or values don't decode).
fn row_values(t: &Table, bix: &BoundIndex, bytes: &[u8], ctx: &ExecCtx) -> Option<Vec<Value>> {
    let row = decode_table_row(t, bytes).ok()?;
    index_values(bix, &row, ctx).ok().flatten()
}

/// Index `bix`'s structure, up to date with every commit (None: it can't have one).
fn structure<P: Placing>(t: &Table, bix: &BoundIndex, sh: &Shape, ctx: &ExecCtx) -> Result<Option<State>> {
    let db = &ctx.sess.db;
    let fp = fingerprint(bix);
    if let Some(st) = db.watch.state(t.oid, crate::txn::watch::slot(bix.index.oid, false)) {
        let mut g = st.lock();
        if g.downcast_ref::<TooBig>().is_some_and(|b| b.0 == fp) {
            return Ok(None);
        }
        if let Some(p) = g.downcast_mut::<P>()
            && p.fingerprint() == fp
            && let Some(writes) = db.watch.pending(t.oid, crate::txn::watch::slot(bix.index.oid, false), &st)
        {
            let nopred = unpredicated(bix);
            for (k, v) in writes {
                if let Some(v) = v {
                    let v = spliced(db, &k, v);
                    p.add(&k[8..], v.and_then(|v| row_values(t, &nopred, &v, ctx)).as_deref());
                }
            }
            if !p.stale() {
                drop(g);
                return Ok(Some(st));
            }
        }
    }
    build::<P>(t, bix, sh, fp, ctx)
}

/// Builds index `bix`'s structure: registered (so every commit after is logged for it),
/// then the table's rows read as of a time every commit before is visible at.
fn build<P: Placing>(t: &Table, bix: &BoundIndex, sh: &Shape, fp: String, ctx: &ExecCtx) -> Result<Option<State>> {
    let db = &ctx.sess.db;
    let st: State = Arc::new(parking_lot::Mutex::new(Box::new(())));
    let mut g = st.lock();
    let last = db.store.with_commit_lock(|last| {
        db.watch.register(t.oid, crate::txn::watch::slot(bix.index.oid, false), st.clone());
        last
    });
    let waited = std::time::Instant::now();
    while db.store.visible_ts() < last {
        if waited.elapsed() > std::time::Duration::from_secs(5) {
            db.watch.forget_index(t.oid, crate::txn::watch::slot(bix.index.oid, false));
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_micros(50));
    }
    let pin = db.store.pin_visible();
    let mut p = P::make(sh.clone(), fp, pin.ts);
    let nopred = unpredicated(bix);
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let budget = budget(ctx);
    let read = (|| -> Result<bool> {
        let mut n = 0usize;
        for r in db.store.scan(&start, &end, pin.ts) {
            let (k, v) = r?;
            ctx.check()?;
            p.add(&k[8..], row_values(t, &nopred, &v, ctx).as_deref());
            n += 1;
            if P::ROW_CAP && (p.full() || (n.is_multiple_of(65536) && p.heap_bytes() > budget)) {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    match read {
        Ok(true) => {}
        // (Too big to hold: the scans read every row, as before, without trying again until
        // the table's log overflows.)
        Ok(false) => {
            *g = Box::new(TooBig(fingerprint(bix)));
            return Ok(None);
        }
        Err(e) => {
            db.watch.forget_index(t.oid, crate::txn::watch::slot(bix.index.oid, false));
            return Err(e);
        }
    }
    p.built();
    tracing::debug!(table = t.oid, index = bix.index.oid, bytes = p.heap_bytes(), "search structure built");
    *g = Box::new(p);
    drop(g);
    Ok(Some(st))
}

/// The rows of `t` (all of them, and maybe more) for which every one of `conds` (index
/// column, operator name, argument) may hold, among those committed as of `snapshot` (None:
/// the latest) and the transaction's own writes. None: the index has no structure, or the
/// conditions narrow nothing: every row is to be read.
pub enum Found {
    /// Data keys, ascending.
    Keys(Vec<Vec<u8>>),
    /// Data key ranges [start, end), ascending, and data keys (the transaction's own
    /// writes) besides.
    Ranges(Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>),
}

pub fn candidates(
    t: &Table,
    bix: &BoundIndex,
    conds: &[(usize, &str, Value)],
    snapshot: Option<u64>,
    ctx: &ExecCtx,
) -> Result<Option<Found>> {
    if matches!(bix.index.method.as_str(), "gin" | "gist") && trigram_index(bix, ctx) {
        return trigram_candidates(t, bix, conds, snapshot, ctx);
    }
    if bix.index.method == "gin" {
        return element_candidates(t, bix, conds, snapshot, ctx);
    }
    let Some(sh) = shape(bix) else { return Ok(None) };
    let mut q = query(&sh, conds);
    let zoned = bix.index.method == "brin";
    if zoned {
        q.part = None;
    }
    if !q.narrowed {
        return Ok(None);
    }
    let st = if zoned { structure::<Zones>(t, bix, &sh, ctx)? } else { structure::<Grid>(t, bix, &sh, ctx)? };
    let Some(st) = st else { return Ok(None) };
    let prefix = index_prefix(t.oid, 0);
    let key = |s: &[u8]| {
        let mut k = prefix.clone();
        k.extend_from_slice(s);
        k
    };
    let mut found = {
        let g = st.lock();
        if let Some(z) = g.downcast_ref::<Zones>() {
            if snapshot.is_some_and(|s| s < z.ts) {
                return Ok(None);
            }
            // (Ends past the last key: `prefix_end`, as storage ranges are bounded.)
            let ranges = z.search(&q).into_iter().map(|(a, b)| (key(a), prefix_end(&key(b)))).collect();
            Found::Ranges(ranges, Vec::new())
        } else if let Some(grid) = g.downcast_ref::<Grid>() {
            if snapshot.is_some_and(|s| s < grid.ts) {
                return Ok(None);
            }
            let mut rows = Vec::new();
            grid.search(&q, &mut rows);
            Found::Keys(rows.into_iter().map(|r| grid.rows.key(&prefix, r)).collect())
        } else {
            return Ok(None);
        }
    };
    // The transaction's own writes (not committed: not in the structure).
    let end = prefix_end(&prefix);
    match &mut found {
        Found::Keys(keys) => {
            match own_rows(t, bix, &sh, &q, ctx)? {
                Some(rows) => keys.extend(rows),
                None => keys.extend(ctx.txn.borrow().writes_in(&prefix, &end)?.into_iter().filter(|(_, v)| v.is_some()).map(|(k, _)| k)),
            }
            keys.sort_unstable();
            keys.dedup();
        }
        Found::Ranges(_, keys) => {
            *keys = ctx.txn.borrow().writes_in(&prefix, &end)?.into_iter().filter(|(_, v)| v.is_some()).map(|(k, _)| k).collect();
        }
    }
    Ok(Some(found))
}

/// A transaction's own writes to the table placed for index `bix` (`Txn::derived`), and
/// the journal position it has read them to.
struct Own {
    pos: usize,
    grid: Grid,
}

/// The data keys of the transaction's own writes `q` may hold for: every version it wrote
/// placed as it reads them from its journal (so a snapshot of an earlier command finds
/// what it sees too). None: its journal was spilled (every own write is to be read).
fn own_rows(t: &Table, bix: &BoundIndex, sh: &Shape, q: &Query, ctx: &ExecCtx) -> Result<Option<Vec<Vec<u8>>>> {
    let prefix = index_prefix(t.oid, 0);
    let (own, new) = {
        let mut txn = ctx.txn.borrow_mut();
        let own = match txn.derived.remove(&bix.index.oid).and_then(|b| b.downcast::<Own>().ok()) {
            Some(o) if o.grid.shape == *sh => *o,
            _ => Own { pos: 0, grid: Grid::new(sh.clone(), String::new(), 0) },
        };
        let pos = txn.journal_pos();
        let Some(new) = txn.written_since(&prefix, &prefix_end(&prefix), own.pos) else {
            return Ok(None);
        };
        (Own { pos, ..own }, new)
    };
    let mut own = own;
    let nopred = unpredicated(bix);
    for (k, now, before) in new {
        for v in [now, before].into_iter().flatten() {
            own.grid.add(&k[8..], row_values(t, &nopred, &v, ctx).as_deref());
        }
    }
    let mut rows = Vec::new();
    own.grid.search(q, &mut rows);
    let keys = rows
        .into_iter()
        .map(|r| own.grid.rows.key(&prefix, r))
        .collect();
    ctx.txn.borrow_mut().derived.insert(bix.index.oid, Box::new(own));
    Ok(Some(keys))
}

/// pg_trgm's operator classes (gin_trgm_ops, gist_trgm_ops) on a text column: rows by the
/// trigrams of their words, as pg_trgm extracts them (a posting list each).
struct Trigrams {
    fingerprint: String,
    ts: u64,
    rows: RowKeys,
    post: HashMap<[u8; 3], Posting>,
    always: Vec<u32>,
    built: usize,
}

impl Placing for Trigrams {
    const ROW_CAP: bool = true;
    fn make(_: Shape, fingerprint: String, ts: u64) -> Trigrams {
        Trigrams { fingerprint, ts, rows: RowKeys::default(), post: HashMap::new(), always: Vec::new(), built: 0 }
    }
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        let row = self.rows.push(suffix);
        match vals.and_then(|v| v.first()) {
            Some(Value::Text(s)) => {
                for t in crate::ext::pg_trgm::trgm::generate(s) {
                    self.post.entry(t).or_default().push(row);
                }
            }
            // (NULL: no operator holds for it.)
            Some(Value::Null) => {}
            _ => self.always.push(row),
        }
    }
    fn built(&mut self) {
        self.built = self.rows.len();
        self.rows.shrink();
        self.post.values_mut().for_each(|l| l.bytes.shrink_to_fit());
    }
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    fn stale(&self) -> bool {
        self.rows.len() > 2 * self.built + 4096
    }
    fn heap_bytes(&self) -> usize {
        // (A hash table entry: its key and list, and a control byte.)
        self.rows.heap_bytes()
            + self.post.capacity() * (std::mem::size_of::<([u8; 3], Posting)>() + 1)
            + self.post.values().map(Posting::heap_bytes).sum::<usize>()
            + self.always.capacity() * 4
    }
    fn full(&self) -> bool {
        self.rows.full()
    }
}

/// Whether index `bix` is a pg_trgm one on one text column.
fn trigram_index(bix: &BoundIndex, ctx: &ExecCtx) -> bool {
    let [c] = &bix.index.columns[..] else { return false };
    matches!(bix.keys.first().map(|k| k.ty()), Some(DataType::Text | DataType::Varchar(_)))
        && ctx.catalog().opclasses.values().any(|o| o.oid == c.opclass && matches!(o.name.as_str(), "gin_trgm_ops" | "gist_trgm_ops"))
}

/// The trigrams every row a LIKE / ILIKE `pattern` (or `=` a value) matches has: those
/// inside each word of its literal parts (pg_trgm's words: runs of ASCII letters and
/// digits, lowered). Fewer than pg_trgm extracts, never more.
fn required_trigrams(op: &str, v: &Value) -> Vec<[u8; 3]> {
    let Value::Text(s) = v else { return Vec::new() };
    if op == "=" {
        return crate::ext::pg_trgm::trgm::generate(s);
    }
    if !matches!(op, "~~" | "~~*") || s.contains('\\') {
        return Vec::new();
    }
    let mut out = Vec::new();
    for w in s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 3) {
        let b = w.to_ascii_lowercase().into_bytes();
        out.extend(b.windows(3).map(|t| [t[0], t[1], t[2]]));
    }
    out
}

/// The least share of a query's trigrams a row has for `op` (`%`, `%>`, `%>>`) to hold at
/// its threshold: similarity and the word similarities are at most the share of the query's
/// trigrams the row has (pg_trgm's GIN consistent function bounds them by it too).
fn similarity_share(op: &str, ctx: &ExecCtx) -> Option<f64> {
    let (name, default) = match op {
        "%" => ("pg_trgm.similarity_threshold", 0.3),
        "%>" => ("pg_trgm.word_similarity_threshold", 0.6),
        "%>>" => ("pg_trgm.strict_word_similarity_threshold", 0.5),
        _ => return None,
    };
    Some(ctx.sess.settings.borrow().get_raw(name).and_then(|v| v.parse().ok()).unwrap_or(default))
}

impl Trigrams {
    fn list(&self, t: &[u8; 3]) -> &Posting {
        self.post.get(t).unwrap_or(&NO_ROWS)
    }

    /// The rows having at least `need` of trigrams `q`.
    fn sharing(&self, q: &[[u8; 3]], need: usize) -> Vec<u32> {
        let lists: Vec<&Posting> = q.iter().map(|t| self.list(t)).collect();
        let total: usize = lists.iter().map(|l| l.len()).sum();
        if total * 4 >= self.rows.len() {
            let mut n = vec![0u16; self.rows.len()];
            for r in lists.iter().flat_map(|l| l.iter()) {
                n[r as usize] = n[r as usize].saturating_add(1);
            }
            return n.iter().enumerate().filter(|(_, c)| **c as usize >= need).map(|(i, _)| i as u32).collect();
        }
        let mut all: Vec<u32> = lists.iter().flat_map(|l| l.iter()).collect();
        all.sort_unstable();
        all.chunk_by(|a, b| a == b).filter(|run| run.len() >= need).map(|run| run[0]).collect()
    }

    /// The rows `op arg` may hold for, through a GIN index (`by_gin`) or a GiST one.
    fn select(&self, op: &str, arg: &Value, by_gin: bool, ctx: &ExecCtx) -> gin::Sel {
        if let (Some(share), Value::Text(s)) = (similarity_share(op, ctx), arg) {
            let q = crate::ext::pg_trgm::trgm::generate(s);
            // (A little under the bound: pg_trgm compares in float4.)
            let mut need = (share * q.len() as f64 - 1e-6 * (q.len() as f64 + 1.0)).ceil().max(0.0) as usize;
            // GIN returns only rows with one of the query's trigrams, and none for a query
            // without any (gin_trgm_consistent: false when nkeys is 0), whatever the
            // threshold; GiST every row a threshold of 0 lets through.
            if by_gin {
                if q.is_empty() {
                    return gin::Sel::Rows(Vec::new());
                }
                need = need.max(1);
            }
            return if need >= 1 { gin::Sel::Rows(self.sharing(&q, need)) } else { gin::Sel::All };
        }
        let mut need = required_trigrams(op, arg);
        need.sort_unstable();
        need.dedup();
        if need.is_empty() {
            return gin::Sel::All;
        }
        gin::Sel::Rows(every(need.iter().map(|t| self.list(t)).collect()))
    }
}

/// An empty posting list.
static NO_ROWS: Posting = Posting { bytes: Vec::new(), last: 0, len: 0 };

/// The rows in every one of `lists` (none: no rows).
fn every(mut lists: Vec<&Posting>) -> Vec<u32> {
    lists.sort_by_key(|l| l.len());
    let Some((first, rest)) = lists.split_first() else { return Vec::new() };
    let mut rows: Vec<u32> = first.iter().collect();
    for l in rest {
        if rows.is_empty() {
            break;
        }
        rows = intersect_posting(&rows, l);
    }
    rows
}

/// `candidates` for a pg_trgm index.
fn trigram_candidates(t: &Table, bix: &BoundIndex, conds: &[(usize, &str, Value)], snapshot: Option<u64>, ctx: &ExecCtx) -> Result<Option<Found>> {
    if !conds.iter().any(|(_, op, v)| similarity_share(op, ctx).is_some() || !required_trigrams(op, v).is_empty()) {
        return Ok(None);
    }
    let sh = Shape { dim: None, parts: Vec::new(), tys: Vec::new() };
    let Some(st) = structure::<Trigrams>(t, bix, &sh, ctx)? else { return Ok(None) };
    let g = st.lock();
    let Some(tg) = g.downcast_ref::<Trigrams>() else { return Ok(None) };
    if snapshot.is_some_and(|s| s < tg.ts) {
        return Ok(None);
    }
    let by_gin = bix.index.method == "gin";
    let sel = conds.iter().fold(gin::Sel::All, |acc, (_, op, v)| acc.and(tg.select(op, v, by_gin, ctx)));
    selected(t, &tg.rows, sel, &tg.always, ctx)
}

/// `candidates` for a GIN index over arrays, jsonb or tsvector (`gin::Elements`).
fn element_candidates(t: &Table, bix: &BoundIndex, conds: &[(usize, &str, Value)], snapshot: Option<u64>, ctx: &ExecCtx) -> Result<Option<Found>> {
    let cols = gin::element_columns(bix, ctx);
    if !conds.iter().any(|(c, _, _)| cols.contains(c)) {
        return Ok(None);
    }
    let sh = Shape { dim: None, parts: cols, tys: Vec::new() };
    let Some(st) = structure::<gin::Elements>(t, bix, &sh, ctx)? else { return Ok(None) };
    let g = st.lock();
    let Some(el) = g.downcast_ref::<gin::Elements>() else { return Ok(None) };
    if snapshot.is_some_and(|s| s < el.ts) {
        return Ok(None);
    }
    let sel = conds.iter().fold(gin::Sel::All, |acc, (c, op, v)| acc.and(el.select(*c, op, v)));
    selected(t, &el.rows, sel, &el.always, ctx)
}

/// The data keys of the rows `sel` selects of `rows`, those always read and the
/// transaction's own writes (every one: not in the structure). None: `sel` narrows nothing.
fn selected(t: &Table, rows: &RowKeys, sel: gin::Sel, always: &[u32], ctx: &ExecCtx) -> Result<Option<Found>> {
    let gin::Sel::Rows(mut sel) = sel else { return Ok(None) };
    sel.extend_from_slice(always);
    let prefix = index_prefix(t.oid, 0);
    let mut keys: Vec<Vec<u8>> = sel.into_iter().map(|r| rows.key(&prefix, r)).collect();
    keys.extend(ctx.txn.borrow().writes_in(&prefix, &prefix_end(&prefix))?.into_iter().filter(|(_, v)| v.is_some()).map(|(k, _)| k));
    keys.sort_unstable();
    keys.dedup();
    Ok(Some(Found::Keys(keys)))
}

