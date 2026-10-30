//! A system catalog's generated rows, as the database's sessions share them (round 19, agent
//! 76): in chunks of whole objects' rows, each chunk shared by pointer between the catalog
//! versions whose rows of those objects are alike (`sysdelta::splice` rebuilds only the
//! chunks holding what a DDL statement changed), with hashes on the columns equalities
//! looked up. A scan copies only the columns its plan reads (`RowChunk::project`, `fill`).
//!
//! A chunk keeps its rows by column, each in the narrowest form its values allow (round 22,
//! agent 115): one value for the whole chunk, a plain vector of oids, integers, booleans or
//! "char"s, or a dictionary of the distinct values. Rows of `Value`s took 32 bytes a column
//! (pg_attribute's 25 columns: 800 bytes a row, and a string allocated per name); 10,000
//! tables' pg_attribute, pg_type and pg_class took 370 MB.

use crate::exec::fasthash::HashMap as FastMap;
use crate::types::Value;
use crate::types::value::Row;
use std::sync::Arc;

/// About how many rows a chunk holds (a chunk ends at the end of an object's rows).
pub const CHUNK_ROWS: usize = 64;

/// A part key (`sysdelta`): the object a run of rows belongs to, or a fixed section.
pub type PartKey = u64;

/// The key of a fixed section's rows (rows no object owns).
pub fn fixed_key(section: u8) -> PartKey {
    (1 << 32) | section as u64
}

/// One column of a chunk's rows.
enum Col {
    /// Every row's value.
    Same(Value),
    Oid(Box<[u32]>),
    Int2(Box<[i16]>),
    Int4(Box<[i32]>),
    Bool(Box<[bool]>),
    Char(Box<[u8]>),
    /// The distinct values, and each row's.
    Dict(Box<[Value]>, Box<[u8]>),
    Any(Box<[Value]>),
}

/// Whether `a` and `b` are one value of one kind, so either may stand for the other.
/// (`Value`'s equality holds across kinds: an int4 and an oid of one number.)
fn exact(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Int2(x), Int2(y)) => x == y,
        (Int4(x), Int4(y)) => x == y,
        (Int8(x), Int8(y)) => x == y,
        (Oid(x), Oid(y)) => x == y,
        (Char(x), Char(y)) => x == y,
        (Float4(x), Float4(y)) => x.to_bits() == y.to_bits(),
        (Float8(x), Float8(y)) => x.to_bits() == y.to_bits(),
        (Text(x), Text(y)) => Arc::ptr_eq(x, y) || x == y,
        (Array(x), Array(y)) => {
            Arc::ptr_eq(x, y)
                || (x.elem == y.elem
                    && x.dims == y.dims
                    && x.items.len() == y.items.len()
                    && x.items.iter().zip(&y.items).all(|(p, q)| exact(p, q)))
        }
        _ => false,
    }
}

impl Col {
    /// Column `k` of `rows` (at least one).
    fn of(rows: &[Row], k: usize) -> Col {
        let first = &rows[0][k];
        // Integers, oids, booleans and "char"s of one kind throughout: a plain vector (or one
        // value), compared as numbers.
        macro_rules! plain {
            ($v:ident) => {{
                let d = std::mem::discriminant(first);
                if rows.iter().all(|r| std::mem::discriminant(&r[k]) == d) {
                    let x: Box<[_]> =
                        rows.iter().map(|r| if let Value::$v(x) = r[k] { x } else { unreachable!() }).collect();
                    if x.iter().all(|v| *v == x[0]) {
                        return Col::Same(first.clone());
                    }
                    return Col::$v(x);
                }
            }};
        }
        match first {
            Value::Oid(_) => plain!(Oid),
            Value::Int2(_) => plain!(Int2),
            Value::Int4(_) => plain!(Int4),
            Value::Bool(_) => plain!(Bool),
            Value::Char(_) => plain!(Char),
            _ => {}
        }
        if rows.iter().all(|r| exact(&r[k], first)) {
            return Col::Same(first.clone());
        }
        let any = || Col::Any(rows.iter().map(|r| r[k].clone()).collect());
        // A dictionary when it holds at most three quarters as many values as the rows.
        let most = (rows.len() * 3 / 4).min(256);
        let mut seen: FastMap<&Value, u8> = FastMap::default();
        let mut dict: Vec<Value> = Vec::new();
        let mut at: Vec<u8> = Vec::with_capacity(rows.len());
        for r in rows {
            let v = &r[k];
            let i = match seen.get(v) {
                Some(&i) if exact(&dict[i as usize], v) => i,
                // (Equal across kinds.)
                Some(_) => return any(),
                None if dict.len() >= most => return any(),
                None => {
                    let i = dict.len() as u8;
                    seen.insert(v, i);
                    dict.push(v.clone());
                    i
                }
            };
            at.push(i);
        }
        Col::Dict(dict.into(), at.into())
    }

    fn get(&self, i: usize) -> Value {
        match self {
            Col::Same(v) => v.clone(),
            Col::Oid(x) => Value::Oid(x[i]),
            Col::Int2(x) => Value::Int2(x[i]),
            Col::Int4(x) => Value::Int4(x[i]),
            Col::Bool(x) => Value::Bool(x[i]),
            Col::Char(x) => Value::Char(x[i]),
            Col::Dict(d, at) => d[at[i] as usize].clone(),
            Col::Any(x) => x[i].clone(),
        }
    }

    /// Row `i`'s value, when the column keeps it as a `Value`.
    fn peek(&self, i: usize) -> Option<&Value> {
        match self {
            Col::Same(v) => Some(v),
            Col::Dict(d, at) => Some(&d[at[i] as usize]),
            Col::Any(x) => Some(&x[i]),
            _ => None,
        }
    }
}

/// Some consecutive rows (by column), with the runs of one part each they make up.
pub struct RowChunk {
    cols: Box<[Col]>,
    len: u32,
    /// (part, the run's end in the rows), in order.
    pub parts: Vec<(PartKey, u32)>,
}

impl RowChunk {
    /// `rows` (all of one width) kept by column.
    pub fn new(rows: Vec<Row>, parts: Vec<(PartKey, u32)>) -> RowChunk {
        let width = rows.first().map_or(0, Vec::len);
        debug_assert!(rows.iter().all(|r| r.len() == width), "a catalog's rows are of one width");
        RowChunk {
            cols: (0..width).map(|k| Col::of(&rows, k)).collect(),
            len: rows.len() as u32,
            parts,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Row `i`, whole.
    pub fn row(&self, i: usize) -> Row {
        self.cols.iter().map(|c| c.get(i)).collect()
    }

    /// Rows `a..b`, whole.
    pub fn rows(&self, a: usize, b: usize) -> impl Iterator<Item = Row> + '_ {
        (a..b).map(|i| self.row(i))
    }

    /// Column `k` of row `i` (NULL past the row's end).
    pub fn value(&self, i: usize, k: usize) -> Value {
        self.cols.get(k).map_or(Value::Null, |c| c.get(i))
    }

    /// `f` of column `k` of row `i` (NULL past the row's end), without a copy where the
    /// column keeps its values.
    pub fn with_value<T>(&self, i: usize, k: usize, f: impl FnOnce(&Value) -> T) -> T {
        match self.cols.get(k).map(|c| (c.peek(i), c)) {
            Some((Some(v), _)) => f(v),
            Some((None, c)) => f(&c.get(i)),
            None => f(&Value::Null),
        }
    }

    /// Whether column `k` of row `i` is NULL (or past the row's end).
    pub fn is_null(&self, i: usize, k: usize) -> bool {
        self.cols.get(k).is_none_or(|c| c.peek(i).is_some_and(Value::is_null))
    }

    /// Row `i`'s columns `mask` marks (all of them without one) set in `out`, which is made
    /// the chunk's width; its other columns are left as they were.
    pub fn fill(&self, i: usize, mask: Option<&[bool]>, out: &mut Row) {
        out.resize(self.cols.len(), Value::Null);
        for (k, c) in self.cols.iter().enumerate() {
            if mask.is_none_or(|m| m.get(k).copied().unwrap_or(false)) {
                out[k] = c.get(i);
            }
        }
    }

    /// Row `i` with only the columns `need` marks (the others NULL: nothing reads them,
    /// `exec::need`); all of them without a mask.
    pub fn project(&self, i: usize, need: Option<&[bool]>) -> Row {
        match need {
            None => self.row(i),
            Some(n) => (self.cols.iter().enumerate())
                .map(|(k, c)| if n.get(k).copied().unwrap_or(false) { c.get(i) } else { Value::Null })
                .collect(),
        }
    }
}

/// `rows` cut into chunks along their parts' runs (`part` of each row), each kept by column
/// as soon as it is cut (the rows it took dropped).
pub fn chunk(rows: Vec<Row>, part: impl Fn(&Row) -> PartKey) -> Vec<Arc<RowChunk>> {
    let mut c = Chunker::default();
    for r in rows {
        let p = part(&r);
        c.push(r, p);
    }
    c.finish()
}

/// Rows cut into chunks as they come (`chunk`'s, a row at a time): a chunk is made once the
/// next part begins and it holds `CHUNK_ROWS` rows, so the last row pushed is always still
/// in `cur`.
#[derive(Default)]
pub struct Chunker {
    out: Vec<Arc<RowChunk>>,
    cur: Vec<Row>,
    parts: Vec<(PartKey, u32)>,
}

impl Chunker {
    pub fn push(&mut self, r: Row, p: PartKey) {
        if self.parts.last().is_none_or(|l| l.0 != p) {
            if self.cur.len() >= CHUNK_ROWS {
                let rows = std::mem::replace(&mut self.cur, Vec::with_capacity(CHUNK_ROWS + 8));
                self.out.push(Arc::new(RowChunk::new(rows, std::mem::take(&mut self.parts))));
            }
            self.parts.push((p, 0));
        }
        self.cur.push(r);
        self.parts.last_mut().unwrap().1 = self.cur.len() as u32;
    }

    pub fn finish(mut self) -> Vec<Arc<RowChunk>> {
        if !self.cur.is_empty() {
            self.out.push(Arc::new(RowChunk::new(self.cur, self.parts)));
        }
        self.out
    }
}

/// Where a catalog's generated rows go (`system::generate`): a vector, or chunks made as the
/// rows come, so a whole catalog's rows (pg_attribute's 600,000 at 90,000 objects) are never
/// all held as rows at once.
pub struct Out<'a> {
    rows: Vec<Row>,
    chunks: Option<(Chunker, &'a dyn Fn(&Row) -> PartKey)>,
}

impl<'a> Out<'a> {
    pub fn rows() -> Out<'a> {
        Out { rows: Vec::new(), chunks: None }
    }

    /// Rows chunked along their parts (`part` of each).
    pub fn chunked(part: &'a dyn Fn(&Row) -> PartKey) -> Out<'a> {
        Out { rows: Vec::new(), chunks: Some((Chunker::default(), part)) }
    }

    pub fn push(&mut self, r: Row) {
        match &mut self.chunks {
            Some((c, part)) => {
                let p = part(&r);
                c.push(r, p)
            }
            None => self.rows.push(r),
        }
    }

    pub fn extend(&mut self, rows: impl IntoIterator<Item = Row>) {
        for r in rows {
            self.push(r);
        }
    }

    /// The row pushed last.
    pub fn last_mut(&mut self) -> Option<&mut Row> {
        match &mut self.chunks {
            Some((c, _)) => c.cur.last_mut(),
            None => self.rows.last_mut(),
        }
    }

    /// Every row so far (only of rows not chunked).
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Row> {
        assert!(self.chunks.is_none(), "rows chunked as they come are changed in place");
        self.rows.iter_mut()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        assert!(self.chunks.is_none(), "rows chunked as they come are read again");
        self.rows.iter()
    }

    /// The rows so far, taken out (only of rows not chunked).
    pub fn drain(&mut self, all: std::ops::RangeFull) -> std::vec::Drain<'_, Row> {
        assert!(self.chunks.is_none(), "rows chunked as they come are taken out again");
        self.rows.drain(all)
    }

    /// The rows made otherwise, in place of any pushed so far.
    pub fn set(&mut self, rows: Vec<Row>) {
        match &mut self.chunks {
            Some((c, _)) => *c = Chunker::default(),
            None => {
                self.rows = rows;
                return;
            }
        }
        self.extend(rows);
    }

    pub fn into_rows(self) -> Vec<Row> {
        debug_assert!(self.chunks.is_none());
        self.rows
    }

    pub fn into_chunks(self) -> Vec<Arc<RowChunk>> {
        match self.chunks {
            Some((c, _)) => c.finish(),
            None => unreachable!("rows not chunked"),
        }
    }
}

pub struct SysRows {
    chunks: Vec<Arc<RowChunk>>,
    /// The first row of each chunk.
    starts: Vec<u32>,
    len: usize,
    /// The column naming the object each row belongs to, when every row whose value there is
    /// a user object's is in that object's run: an equality on it is served by `runs`.
    owner: Option<usize>,
    /// Per owner: where its rows lie.
    runs: std::sync::OnceLock<FastMap<u32, (u32, u32)>>,
    /// Per column: the rows holding each value.
    by_col: parking_lot::Mutex<FastMap<usize, Arc<FastMap<Value, Vec<u32>>>>>,
    /// Per column and kind of value: whether every row's value is of that kind (or NULL),
    /// so the hash serves an equality with it.
    hashable: parking_lot::Mutex<FastMap<(usize, std::mem::Discriminant<Value>), bool>>,
    /// Per column: the rows where it is NULL.
    nulls: parking_lot::Mutex<FastMap<usize, Arc<Vec<u32>>>>,
}

impl SysRows {
    /// A remade chunk's rows and runs (`sysdelta::splice`), cut again where it has grown past
    /// twice the size (rows added at one place by statement after statement).
    pub fn rechunk(rows: Vec<Row>, parts: Vec<(PartKey, u32)>) -> Vec<Arc<RowChunk>> {
        if rows.len() <= 2 * CHUNK_ROWS {
            return vec![Arc::new(RowChunk::new(rows, parts))];
        }
        let mut out = Vec::new();
        let mut it = rows.into_iter();
        let mut cur: Vec<Row> = Vec::new();
        let mut cur_parts: Vec<(PartKey, u32)> = Vec::new();
        let mut a = 0;
        for (p, end) in parts {
            if cur.len() >= CHUNK_ROWS {
                out.push(Arc::new(RowChunk::new(std::mem::take(&mut cur), std::mem::take(&mut cur_parts))));
            }
            cur.extend(it.by_ref().take((end - a) as usize));
            cur_parts.push((p, cur.len() as u32));
            a = end;
        }
        if !cur.is_empty() {
            out.push(Arc::new(RowChunk::new(cur, cur_parts)));
        }
        out
    }

    /// Rows of no parts (a catalog whose rows aren't spliced).
    pub fn new(rows: Vec<Row>) -> SysRows {
        SysRows::from_chunks(chunk(rows, |_| fixed_key(0)), None)
    }

    pub fn from_chunks(chunks: Vec<Arc<RowChunk>>, owner: Option<usize>) -> SysRows {
        let mut starts = Vec::with_capacity(chunks.len());
        let mut len = 0;
        for c in &chunks {
            starts.push(len as u32);
            len += c.len();
        }
        SysRows {
            chunks,
            starts,
            len,
            owner,
            runs: Default::default(),
            by_col: Default::default(),
            hashable: Default::default(),
            nulls: Default::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn chunks(&self) -> &[Arc<RowChunk>] {
        &self.chunks
    }

    /// Row `i` (in row order): its chunk, and its place there.
    pub fn locate(&self, i: usize) -> (&RowChunk, usize) {
        let c = self.starts.partition_point(|&s| s as usize <= i) - 1;
        (&self.chunks[c], i - self.starts[c] as usize)
    }

    /// Row `i` (in row order), whole.
    pub fn row(&self, i: usize) -> Row {
        let (c, k) = self.locate(i);
        c.row(k)
    }

    /// Every row, whole, in order.
    pub fn iter(&self) -> impl Iterator<Item = Row> + '_ {
        self.chunks.iter().flat_map(|c| c.rows(0, c.len()))
    }

    /// Every row's place: (its number, its chunk, its place there), in order.
    pub fn places(&self) -> impl Iterator<Item = (usize, &RowChunk, usize)> + '_ {
        (self.chunks.iter().zip(&self.starts))
            .flat_map(|(c, s)| (0..c.len()).map(move |i| (*s as usize + i, &**c, i)))
    }

    /// The rows of user object `o` in column `owner` (None: not served so, or a system
    /// object's, which a fixed section may hold).
    fn run_of(&self, o: u32) -> Option<std::ops::Range<u32>> {
        self.owner?;
        if o < super::FIRST_USER_OID {
            return None;
        }
        let runs = self.runs.get_or_init(|| {
            let mut m: FastMap<u32, (u32, u32)> = FastMap::default();
            for (c, start) in self.chunks.iter().zip(&self.starts) {
                let mut a = 0;
                for &(p, end) in &c.parts {
                    if p >> 32 == 0 {
                        m.insert(p as u32, (start + a, start + end));
                    }
                    a = end;
                }
            }
            m
        });
        let (a, b) = runs.get(&o).copied().unwrap_or((0, 0));
        Some(a..b)
    }

    /// The rows whose column `k` is NULL, in row order.
    pub fn nulls(&self, k: usize) -> Arc<Vec<u32>> {
        self.nulls
            .lock()
            .entry(k)
            .or_insert_with(|| {
                Arc::new(self.places().filter(|&(_, c, i)| c.is_null(i, k)).map(|(n, ..)| n as u32).collect())
            })
            .clone()
    }

    /// The rows whose column `k` equals `v` (in row order), when a hash on the column can
    /// tell (every value of the column is of `v`'s kind or NULL); None otherwise.
    pub fn lookup(&self, k: usize, v: &Value, same: &dyn Fn(&Value) -> bool) -> Option<Vec<u32>> {
        if let Value::Oid(o) = v
            && self.owner == Some(k)
            && same(v)
            && let Some(run) = self.run_of(*o)
        {
            return Some(run.collect());
        }
        let kind = (k, std::mem::discriminant(v));
        let ok = *self.hashable.lock().entry(kind).or_insert_with(|| {
            self.places().all(|(_, c, i)| {
                let x = c.value(i, k);
                k < c.cols.len() && (x.is_null() || same(&x))
            })
        });
        if !ok {
            return None;
        }
        let map = self
            .by_col
            .lock()
            .entry(k)
            .or_insert_with(|| {
                let mut m: FastMap<Value, Vec<u32>> = FastMap::default();
                for (n, c, i) in self.places() {
                    let x = c.value(i, k);
                    if !x.is_null() {
                        m.entry(x).or_default().push(n as u32);
                    }
                }
                Arc::new(m)
            })
            .clone();
        Some(map.get(v).cloned().unwrap_or_default())
    }
}
