//! A system catalog's generated rows, as the database's sessions share them (round 19, agent
//! 76): in chunks of whole objects' rows, each chunk shared by pointer between the catalog
//! versions whose rows of those objects are alike (`sysdelta::splice` rebuilds only the
//! chunks holding what a DDL statement changed), with hashes on the columns equalities
//! looked up. A scan copies only the columns its plan reads (`project`).

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

/// Some consecutive rows, with the runs of one part each they make up.
pub struct RowChunk {
    pub rows: Vec<Row>,
    /// (part, the run's end in `rows`), in order.
    pub parts: Vec<(PartKey, u32)>,
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

/// Row `r` with only the columns `need` marks copied (the others NULL: nothing reads them,
/// `exec::need`); a copy of all of it without a mask.
pub fn project(r: &Row, need: Option<&[bool]>) -> Row {
    match need {
        None => r.clone(),
        Some(n) => (r.iter().enumerate())
            .map(|(i, v)| if n.get(i).copied().unwrap_or(false) { v.clone() } else { Value::Null })
            .collect(),
    }
}

/// `rows` cut into chunks along their parts' runs (`part` of each row).
pub fn chunk(rows: Vec<Row>, part: impl Fn(&Row) -> PartKey) -> Vec<Arc<RowChunk>> {
    let mut out = Vec::with_capacity(rows.len() / CHUNK_ROWS + 1);
    let mut cur = RowChunk { rows: Vec::with_capacity(CHUNK_ROWS + 8), parts: Vec::new() };
    for r in rows {
        let p = part(&r);
        if cur.parts.last().is_none_or(|l| l.0 != p) {
            if cur.rows.len() >= CHUNK_ROWS {
                out.push(Arc::new(std::mem::replace(
                    &mut cur,
                    RowChunk { rows: Vec::with_capacity(CHUNK_ROWS + 8), parts: Vec::new() },
                )));
            }
            cur.parts.push((p, 0));
        }
        cur.rows.push(r);
        cur.parts.last_mut().unwrap().1 = cur.rows.len() as u32;
    }
    if !cur.rows.is_empty() {
        out.push(Arc::new(cur));
    }
    out
}

impl SysRows {
    /// A remade chunk's rows and runs (`sysdelta::splice`), cut again where it has grown past
    /// twice the size (rows added at one place by statement after statement).
    pub fn rechunk(rows: Vec<Row>, parts: Vec<(PartKey, u32)>) -> Vec<Arc<RowChunk>> {
        if rows.len() <= 2 * CHUNK_ROWS {
            return vec![Arc::new(RowChunk { rows, parts })];
        }
        let mut out = Vec::new();
        let mut it = rows.into_iter();
        let mut cur = RowChunk { rows: Vec::new(), parts: Vec::new() };
        let mut a = 0;
        for (p, end) in parts {
            if cur.rows.len() >= CHUNK_ROWS {
                out.push(Arc::new(std::mem::replace(&mut cur, RowChunk { rows: Vec::new(), parts: Vec::new() })));
            }
            cur.rows.extend(it.by_ref().take((end - a) as usize));
            cur.parts.push((p, cur.rows.len() as u32));
            a = end;
        }
        if !cur.rows.is_empty() {
            out.push(Arc::new(cur));
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
            len += c.rows.len();
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

    /// Row `i` (in row order).
    pub fn row(&self, i: usize) -> &Row {
        let c = self.starts.partition_point(|&s| s as usize <= i) - 1;
        &self.chunks[c].rows[i - self.starts[c] as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = &Row> + '_ {
        self.chunks.iter().flat_map(|c| c.rows.iter())
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
                Arc::new(
                    (self.iter().enumerate())
                        .filter(|(_, r)| r.get(k).is_none_or(Value::is_null))
                        .map(|(i, _)| i as u32)
                        .collect(),
                )
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
        let ok = *self
            .hashable
            .lock()
            .entry(kind)
            .or_insert_with(|| self.iter().all(|r| r.get(k).is_some_and(|x| x.is_null() || same(x))));
        if !ok {
            return None;
        }
        let map = self
            .by_col
            .lock()
            .entry(k)
            .or_insert_with(|| {
                let mut m: FastMap<Value, Vec<u32>> = FastMap::default();
                for (i, r) in self.iter().enumerate() {
                    if !r[k].is_null() {
                        m.entry(r[k].clone()).or_default().push(i as u32);
                    }
                }
                Arc::new(m)
            })
            .clone();
        Some(map.get(v).cloned().unwrap_or_default())
    }
}
