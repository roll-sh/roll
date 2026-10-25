//! Relation files as Postgres would have them on disk. Every table's heap
//! (`catalog::heapsim`) and btree index (`catalog::btsim`) is modelled alongside the rows
//! written, so pg_relation_size and friends, VACUUM's and ANALYZE's relpages and the
//! planner's size estimates come out as Postgres's do.
//!
//! The models live in the database (`Database::relfiles`); a relation not modelled yet
//! (written before the engine started, or by a path that doesn't report here) is laid out
//! afresh from its rows the first time it is needed.

use super::ExecCtx;
use super::dml::index_values;
use super::scan::decode_table_row;
use crate::catalog::btsim::{Btree, BtreeOpts, InsertHint, Key};
use crate::catalog::heapsim::{self, Heap, InsertMode, Tid, Vis};
use crate::catalog::{Catalog, Table, relsize};
use crate::error::{Error, Result, code};
use crate::sql::expr::Expr;
use crate::sql::plan::BoundIndex;
use crate::storage::keys::{encode_value, index_prefix, prefix_end};
use crate::txn::Database;
use crate::txn::xids::Xids;
use crate::types::{DataType, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Tables with more row versions than this aren't modelled (their sizes are estimated).
const MAX_VERSIONS: usize = 400_000;
/// COPY's multi-insert buffer limits (MAX_BUFFERED_TUPLES, MAX_BUFFERED_BYTES).
const COPY_TUPLES: usize = 1000;
const COPY_BYTES: usize = 65535;

#[derive(Default)]
pub struct RelFiles {
    heaps: HashMap<u32, Heap>,
    btrees: HashMap<u32, Btree>,
    /// Out-of-line values of each table's rows, by table.
    toast: HashMap<u32, Toast>,
    /// Tables too big to model.
    untracked: HashSet<u32>,
    /// Their heaps' estimated sizes.
    estimated: HashMap<u32, EstHeap>,
    /// Bulk loads (COPY) in progress, by (table, transaction).
    bulk: HashMap<(u32, u64), Bulk>,
    /// Hash indexes: the heap's estimated tuples when each was built (its initial
    /// buckets).
    hash_est: HashMap<u32, f64>,
    /// GIN indexes: the rows (data keys) their build read; rows written later went to the
    /// pending list.
    gin_built: HashMap<u32, Vec<HashSet<Vec<u8>>>>,
    /// BRIN indexes: which page ranges have a summary.
    brin: HashMap<u32, BrinRanges>,
}

/// A BRIN index's summarized page ranges: those its build covered (the heap's pages then,
/// at least the first range), plus those summarized since, less those desummarized.
#[derive(Clone, Default)]
struct BrinRanges {
    built_blocks: u32,
    added: std::collections::BTreeSet<u32>,
    removed: std::collections::BTreeSet<u32>,
}

impl BrinRanges {
    fn summarized(&self, range: u32, ppr: u32) -> bool {
        let built = self.built_blocks.max(1).div_ceil(ppr);
        self.added.contains(&range) || range < built && !self.removed.contains(&range)
    }
}

/// A table too big to model (`untracked`): its heap's size as Postgres's file would have
/// it, estimated by putting every row version written after the last one (into the space a
/// VACUUM found free first). What the planner's current page count, VACUUM's and ANALYZE's
/// relpages and pg_relation_size read for it.
#[derive(Clone, Copy, Default)]
struct EstHeap {
    pages: u32,
    /// Free bytes in the last page.
    room: usize,
    /// Free bytes in the pages before it.
    reusable: usize,
}

impl EstHeap {
    fn from_heap(h: &Heap) -> EstHeap {
        EstHeap {
            pages: h.nblocks(),
            room: h.last_page_free(),
            reusable: 0,
        }
    }

    /// A tuple of `len` bytes written (RelationGetBufferForTuple keeps a page's fillfactor
    /// free for updates; a new page takes any tuple).
    fn add(&mut self, len: usize, fillfactor: usize) {
        let need = heapsim::maxalign(len) + heapsim::ITEM_ID;
        if self.reusable >= need {
            self.reusable -= need;
            return;
        }
        let reserve = heapsim::BLCKSZ * (100 - fillfactor.min(100)) / 100;
        if self.pages == 0 || need + reserve > self.room {
            self.pages += 1;
            self.room = heapsim::BLCKSZ - heapsim::PAGE_HEADER;
        }
        self.room = self.room.saturating_sub(need);
    }
}

/// The estimate for rows written in one go (a load, a rewrite).
fn estimate_rows<'a>(t: &Table, rows: impl Iterator<Item = &'a [Value]>, ctx: &ExecCtx) -> EstHeap {
    let ff = heap_fillfactor(t);
    let mut e = EstHeap::default();
    for r in rows {
        e.add(layout(t, r, ctx, &HashSet::new()).0, ff);
    }
    e
}

/// An untracked table's row version of `row` was written.
fn estimate_grows(t: &Table, row: &[Value], ctx: &ExecCtx) {
    let len = layout(t, row, ctx, &HashSet::new()).0;
    if let Some(e) = ctx.sess.db.relfiles.lock().estimated.get_mut(&t.oid) {
        e.add(len, heap_fillfactor(t));
    }
}

/// Whether table `t` is too big to model (its sizes are estimates).
pub fn is_estimated(t: u32, db: &Database) -> bool {
    db.relfiles.lock().untracked.contains(&t)
}

/// VACUUM, ANALYZE or CREATE INDEX measured an untracked table's live rows at `live` pages
/// packed afresh: the space beyond them is free for new row versions. Its estimated pages
/// (the file doesn't shrink), None when it is modelled.
pub fn measured_live(t: u32, live: u32, db: &Database) -> Option<u32> {
    let mut rf = db.relfiles.lock();
    let e = rf.estimated.get_mut(&t)?;
    e.pages = e.pages.max(live);
    e.reusable = (e.pages - live) as usize * (heapsim::BLCKSZ - heapsim::PAGE_HEADER);
    Some(e.pages)
}

struct Bulk {
    mode: InsertMode,
    /// COPY's multi-insert buffer (None: rows go in one at a time).
    batch: Option<Batch>,
}

#[derive(Default)]
struct Batch {
    rows: Vec<Pending>,
    bytes: usize,
}

/// A buffered row: its heap tuple's length, its out-of-line values and its btree entries.
struct Pending {
    key: Vec<u8>,
    len: usize,
    exts: Vec<NewExt>,
    entries: Vec<(u32, Arc<Key>)>,
}

/// A value a row's tuple stores out of line: its column, the bytes saved in TOAST chunks
/// and a fingerprint of the value (an UPDATE that keeps it keeps the chunks).
#[derive(Clone, Debug)]
pub struct NewExt {
    col: usize,
    bytes: usize,
    sig: u64,
}

/// A saved out-of-line value: its chunk_id and chunk count.
#[derive(Clone, Debug)]
struct Ext {
    col: usize,
    id: u64,
    bytes: usize,
    sig: u64,
}

impl Ext {
    fn chunks(&self) -> u32 {
        self.bytes.div_ceil(TOAST_CHUNK) as u32
    }
}

/// A table's TOAST values, by the row they belong to.
#[derive(Default)]
struct Toast {
    next_id: u64,
    rows: HashMap<Vec<u8>, Vec<Ext>>,
}

/// TOAST_TUPLE_THRESHOLD and TOAST_TUPLE_TARGET.
const TOAST_THRESHOLD: usize = 2032;
/// TOAST_TUPLE_TARGET_MAIN.
const TOAST_TARGET_MAIN: usize = 8160;
/// TOAST_MAX_CHUNK_SIZE.
const TOAST_CHUNK: usize = 1996;
/// An on-disk TOAST pointer (a 1-byte header and varatt_external).
const TOAST_POINTER: usize = 18;

/// A storage parameter's value.
fn option(opts: &[String], name: &str) -> Option<String> {
    opts.iter().find_map(|o| {
        let (k, v) = o.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().trim_matches('\'').to_string())
    })
}

/// A table's fillfactor (percent).
pub fn heap_fillfactor(t: &Table) -> usize {
    option(&t.options, "fillfactor")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

/// How a btree index behaves (None: another access method).
fn btree_opts(bix: &BoundIndex) -> Option<BtreeOpts> {
    let ix = &bix.index;
    if ix.method != "btree" {
        return None;
    }
    let dedup_on = option(&ix.options, "deduplicate_items").is_none_or(|v| {
        !matches!(
            v.to_ascii_lowercase().as_str(),
            "off" | "false" | "0" | "no" | "of"
        )
    });
    Some(BtreeOpts {
        nkey: bix.keys.len(),
        unique: ix.unique,
        dedup: dedup_on
            && ix.include.is_empty()
            && bix.keys.iter().all(|k| relsize::equal_image(&k.ty())),
        fillfactor: option(&ix.options, "fillfactor")
            .and_then(|v| v.parse().ok())
            .unwrap_or(90),
    })
}

/// A TOAST table's index: (chunk_id, chunk_seq), unique.
fn toast_index_opts() -> BtreeOpts {
    BtreeOpts {
        nkey: 2,
        unique: true,
        dedup: true,
        fillfactor: 90,
    }
}

fn toast_key(id: u64, seq: u32) -> Vec<u8> {
    let mut k = id.to_be_bytes().to_vec();
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

fn toast_index_key(id: u64, seq: u32) -> Arc<Key> {
    Arc::new(Key::new(
        &[id.to_be_bytes().to_vec(), seq.to_be_bytes().to_vec()],
        &[16, 16],
    ))
}

/// A TOAST chunk tuple's t_len: (chunk_id oid, chunk_seq int4, chunk_data bytea).
fn chunk_len(bytes: usize, seq: u32) -> usize {
    let size = (bytes - seq as usize * TOAST_CHUNK).min(TOAST_CHUNK);
    32 + if size <= 126 { 1 + size } else { 4 + size }
}

/// The compression method each stored value was compressed with, by table, column and the
/// value's signature: a compressed datum is copied as it is (into another table, a new
/// version), so it keeps the method it got then, whatever the column's is now. Each value's
/// entries are per stored row (`RowId`): rows of the same content stored apart keep their
/// own methods.
/// (By database too, `Database::instance`: table oids, transaction ids and row keys repeat
/// across the engines and stores of a process.)
#[allow(clippy::type_complexity)]
static METHODS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<(u64, u32, usize, u64), Vec<(RowId, char)>>>,
> = std::sync::LazyLock::new(Default::default);

/// The database of the statement running on this thread (`METHODS`'s key).
fn methods_db() -> u64 {
    crate::session::current::with_current(|ctx| ctx.sess.db.instance).unwrap_or(0)
}

/// Which stored row a value is in: the version's xmin and, when known, the row's key.
type RowId = (u64, Option<Arc<[u8]>>);

/// The method of the entries `e` for the row `at` (else one of its version, else the
/// latest stored).
fn method_of(e: &[(RowId, char)], at: Option<&RowId>) -> Option<char> {
    let by_key = at.and_then(|(x, k)| {
        let k = k.as_ref()?;
        e.iter()
            .rev()
            .find(|((m, l), _)| m == x && l.as_ref() == Some(k))
    });
    let by_xmin = || at.and_then(|(x, _)| e.iter().rev().find(|((m, _), _)| m == x));
    by_key.or_else(by_xmin).or(e.last()).map(|(_, c)| *c)
}

/// Record `method` for the row `at` (`replace`: over one recorded for it before).
fn record_method(e: &mut Vec<(RowId, char)>, at: RowId, method: char, replace: bool) {
    if let Some(i) = e.iter().position(|(r, _)| *r == at) {
        if replace {
            e[i].1 = method;
        }
        return;
    }
    e.push((at, method));
}

/// A large value read from a table, by its allocation (alive while the reference is).
enum ReadRef {
    Str(std::sync::Weak<str>),
    Bytes(std::sync::Weak<[u8]>),
}

/// Values shorter than this are never compressed; longer ones aren't tracked.
const READ_MIN: usize = 32;
const READ_MAX: usize = 1 << 20;
const READ_CAP: usize = 256;

thread_local! {
    /// Large values recently read from tables, with the table and column: one stored again
    /// unchanged (the same allocation) is that column's datum, copied.
    static READ: std::cell::RefCell<std::collections::VecDeque<(ReadRef, u32, usize, RowId)>> =
        Default::default();
}

/// Tables (by database instance) that may hold compressed or out-of-line datums: only
/// values read from them are worth remembering (`note_read`). Bumped generation on a new one.
static TRACKED: std::sync::LazyLock<std::sync::Mutex<HashSet<(u64, u32)>>> =
    std::sync::LazyLock::new(Default::default);
static TRACKED_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// The last answer of `tracked`: (generation, database, table, tracked).
    static TRACKED_LAST: std::cell::Cell<(u64, u64, u32, bool)> =
        const { std::cell::Cell::new((0, 0, 0, false)) };
}

/// Table `t` of database `db` may store values compressed or out of line from now on.
fn mark_tracked(db: u64, t: u32) {
    if let Ok(mut s) = TRACKED.lock()
        && s.insert((db, t))
    {
        TRACKED_GEN.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Whether values read from table `t` (of the running statement's database) may be
/// compressed or out-of-line datums.
pub(crate) fn tracked(t: u32) -> bool {
    let generation = TRACKED_GEN.load(std::sync::atomic::Ordering::Acquire);
    if generation == 0 {
        return false;
    }
    let db = methods_db();
    TRACKED_LAST.with(|c| {
        let (g, d, tt, b) = c.get();
        if g == generation && d == db && tt == t {
            return b;
        }
        let b = TRACKED.lock().is_ok_and(|s| s.contains(&(db, t)));
        c.set((generation, db, t, b));
        b
    })
}

/// A stored row of table `t`, the version `xmin` wrote (under `key`, when known), was read.
pub fn note_read(t: u32, xmin: u64, key: Option<&[u8]>, row: &[Value]) {
    // (Nothing in a table that never stored a datum compressed or out of line is.)
    if !tracked(t) {
        return;
    }
    // (The key is copied once, for the first value tracked.)
    let mut shared: Option<Option<Arc<[u8]>>> = None;
    for (i, v) in row.iter().enumerate() {
        let r = match v {
            Value::Text(s) | Value::Json(s) if (READ_MIN..READ_MAX).contains(&s.len()) => {
                ReadRef::Str(Arc::downgrade(s))
            }
            Value::Bytea(b) if (READ_MIN..READ_MAX).contains(&b.len()) => {
                ReadRef::Bytes(Arc::downgrade(b))
            }
            _ => continue,
        };
        let key = shared.get_or_insert_with(|| key.map(Arc::from)).clone();
        READ.with(|q| {
            let mut q = q.borrow_mut();
            if q.len() >= READ_CAP {
                q.pop_front();
            }
            q.push_back((r, t, i, (xmin, key)));
        });
    }
}

/// The table, column and row version `v` was read from, when it is a stored datum carried
/// along unchanged.
fn read_from(v: &Value) -> Option<(u32, usize, RowId)> {
    let (Value::Text(_) | Value::Json(_) | Value::Bytea(_)) = v else {
        return None;
    };
    READ.with(|q| {
        q.borrow().iter().rev().find_map(|(r, t, i, x)| {
            let same = match (r, v) {
                (ReadRef::Str(w), Value::Text(s) | Value::Json(s)) => {
                    std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(s))
                }
                (ReadRef::Bytes(w), Value::Bytea(b)) => std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(b)),
                _ => false,
            };
            same.then(|| (*t, *i, x.clone()))
        })
    })
}

/// The method `v` is compressed with when it is a datum copied from a table storing it
/// compressed.
fn compressed_with(v: &Value) -> Option<char> {
    let (t, i, x) = read_from(v)?;
    method_of(METHODS.lock().ok()?.get(&(methods_db(), t, i, value_sig(v)))?, Some(&x))
}

/// The method a datum read from a relation is stored compressed with.
pub fn read_compression(v: &Value) -> Option<char> {
    compressed_with(v)
}

/// The method `v`, stored in table `t`'s column `col`, was compressed with.
fn stored_with(t: u32, col: usize, v: &Value) -> Option<char> {
    method_of(METHODS.lock().ok()?.get(&(methods_db(), t, col, value_sig(v)))?, None)
}

/// A row about to be stored in `t`: the values it compresses get their method remembered.
pub fn note_compression(t: &Table, row: &[Value], key: Option<&[u8]>, ctx: &ExecCtx) {
    let n = row.len().min(t.columns.len());
    // A compressed datum copied here stays compressed, as it was.
    let copied: Vec<(usize, u64, char)> = row[..n]
        .iter()
        .enumerate()
        .filter_map(|(i, v)| compressed_with(v).map(|m| (i, value_sig(v), m)))
        .collect();
    let at: RowId = (ctx.txn.borrow_mut().cur_xid(), key.map(Arc::from));
    let db = ctx.sess.db.instance;
    if !copied.is_empty() {
        mark_tracked(db, t.oid);
    }
    if !copied.is_empty()
        && let Ok(mut m) = METHODS.lock()
    {
        // (The latest copy wins over a value of the same content stored before.)
        for (i, sig, method) in copied {
            record_method(m.entry((db, t.oid, i, sig)).or_default(), at.clone(), method, true);
        }
    }
    let types: Vec<DataType> = t.columns[..n].iter().map(|c| c.ty.clone()).collect();
    let fmt = ctx.fmt();
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
    let row: Vec<Value> = t.columns[..n]
        .iter()
        .zip(row)
        .map(|(c, v)| if c.virtual_gen || c.dropped { Value::Null } else { v.clone() })
        .collect();
    if relsize::heap_tuple_len(&types, &row, &text) <= TOAST_THRESHOLD {
        return;
    }
    // (Compressed or moved out of line.)
    mark_tracked(db, t.oid);
    let lz4 = ctx
        .sess
        .settings
        .borrow()
        .get_raw("default_toast_compression")
        .is_some_and(|v| v == "lz4");
    let (_, _, st, zipped) = toast_layout_full(t, &types, &row, &HashSet::new(), &text, lz4);
    let Ok(mut m) = METHODS.lock() else { return };
    for i in 0..n {
        if matches!(st[i], St::Compressed(_)) || (st[i] == St::External && zipped[i]) {
            let method = match t.columns[i].compression {
                Some(c) => c,
                None if lz4 => 'l',
                None => 'p',
            };
            record_method(m.entry((db, t.oid, i, value_sig(&row[i]))).or_default(), at.clone(), method, false);
        }
    }
}

fn value_sig(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut b = Vec::new();
    encode_value(&mut b, v, false);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

/// A value's bytes as a varlena stores them (None: a binary form not modelled).
fn raw_bytes(v: &Value, ty: &DataType) -> Option<Vec<u8>> {
    match v {
        Value::Text(s) => {
            let mut b = s.as_bytes().to_vec();
            if let DataType::Bpchar(m) = ty
                && *m > 4
            {
                let want = (*m - 4) as usize;
                b.resize(b.len() + want.saturating_sub(s.chars().count()), b' ');
            }
            Some(b)
        }
        Value::Json(s) if !matches!(ty, DataType::Jsonb) => Some(s.as_bytes().to_vec()),
        Value::Json(s) => Some(crate::catalog::jsonbin::jsonb_bytes(s)),
        Value::Jsonb(j) => Some(crate::catalog::jsonbin::jsonb_bytes(&j.to_string())),
        Value::Bytea(b) => Some(b.to_vec()),
        Value::Array(a) => array_bytes(a, ty),
        _ => None,
    }
}

/// An array's bytes after its varlena header (construct_md_array's layout): ndim, data
/// offset, element type, dimensions, lower bounds, null bitmap, then the elements aligned
/// (positions counted from the varlena's start).
fn array_bytes(a: &crate::types::array::ArrayValue, ty: &DataType) -> Option<Vec<u8>> {
    let elem = ty.elem().cloned().unwrap_or_else(|| a.elem.clone());
    let (len, al) = crate::catalog::relsize::type_layout(&elem);
    let ndim = a.dims.len();
    let has_null = a.items.iter().any(Value::is_null);
    let bitmap = if has_null { a.items.len().div_ceil(8) } else { 0 };
    let overhead = (16 + 8 * ndim + bitmap).div_ceil(8) * 8;
    let mut out = Vec::new();
    out.extend_from_slice(&(ndim as i32).to_le_bytes());
    out.extend_from_slice(&(if has_null { overhead as i32 } else { 0 }).to_le_bytes());
    out.extend_from_slice(&elem.oid().to_le_bytes());
    for (n, _) in &a.dims {
        out.extend_from_slice(&(*n as i32).to_le_bytes());
    }
    for (_, lb) in &a.dims {
        out.extend_from_slice(&lb.to_le_bytes());
    }
    if has_null {
        let mut bits = vec![0u8; bitmap];
        for (i, x) in a.items.iter().enumerate() {
            if !x.is_null() {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        out.extend(bits);
    }
    out.resize(overhead - 4, 0);
    for x in a.items.iter().filter(|x| !x.is_null()) {
        // (Alignment counts the varlena header's 4 bytes.)
        let pos = out.len() + 4;
        out.resize(pos.div_ceil(al) * al - 4, 0);
        let fixed: Option<Vec<u8>> = match x {
            Value::Bool(b) => Some(vec![*b as u8]),
            Value::Int2(v) => Some(v.to_le_bytes().to_vec()),
            Value::Int4(v) | Value::Date(v) => Some(v.to_le_bytes().to_vec()),
            Value::Int8(v) | Value::Time(v) | Value::Timestamp(v) | Value::Timestamptz(v) => {
                Some(v.to_le_bytes().to_vec())
            }
            Value::Float4(v) => Some(v.to_bits().to_le_bytes().to_vec()),
            Value::Float8(v) => Some(v.to_bits().to_le_bytes().to_vec()),
            Value::Oid(v) => Some(v.to_le_bytes().to_vec()),
            _ => None,
        };
        match fixed {
            Some(b) if len > 0 && b.len() == len as usize => out.extend(b),
            Some(_) => return None,
            None if len > 0 => return None,
            None => {
                // A varlena element keeps its 4-byte header.
                let b = raw_bytes(x, &elem)?;
                out.extend_from_slice(&(((b.len() + 4) as u32) << 2).to_le_bytes());
                out.extend(b);
            }
        }
    }
    Some(out)
}

/// A heap tuple's length (t_len) for `row`, with the values heap_toast_insert_or_update
/// would store out of line (`pinned`: columns keeping an out-of-line value as they are).
pub fn layout(
    t: &Table,
    row: &[Value],
    ctx: &ExecCtx,
    pinned: &HashSet<usize>,
) -> (usize, Vec<NewExt>) {
    let n = row.len().min(t.columns.len());
    let types: Vec<DataType> = t.columns[..n].iter().map(|c| c.ty.clone()).collect();
    let fmt = ctx.fmt();
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
    // Virtual generated and dropped columns are stored as nulls.
    let nulled: Vec<Value>;
    let row: &[Value] = if t.columns[..n]
        .iter()
        .zip(row)
        .any(|(c, v)| (c.virtual_gen || c.dropped) && !v.is_null())
    {
        nulled = t.columns[..n]
            .iter()
            .zip(row)
            .map(|(c, v)| {
                if c.virtual_gen || c.dropped {
                    Value::Null
                } else {
                    v.clone()
                }
            })
            .collect();
        &nulled
    } else {
        &row[..n]
    };
    let len = relsize::heap_tuple_len(&types, row, &text);
    if len <= TOAST_THRESHOLD && pinned.is_empty() {
        return (len, Vec::new());
    }
    let lz4 = ctx
        .sess
        .settings
        .borrow()
        .get_raw("default_toast_compression")
        .is_some_and(|v| v == "lz4");
    toast_layout(t, &types, row, pinned, &text, lz4)
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Inline,
    /// Compressed inline: its varlena size.
    Compressed(usize),
    External,
}

/// heap_toast_insert_or_update: compress and move values out of line until the tuple fits.
fn toast_layout(
    t: &Table,
    types: &[DataType],
    row: &[Value],
    pinned: &HashSet<usize>,
    text: &dyn Fn(&Value, &DataType) -> String,
    default_lz4: bool,
) -> (usize, Vec<NewExt>) {
    let (len, exts, _, _) = toast_layout_full(t, types, row, pinned, text, default_lz4);
    (len, exts)
}

/// toast_layout, with each column's outcome and whether an out-of-line one was compressed.
fn toast_layout_full(
    t: &Table,
    types: &[DataType],
    row: &[Value],
    pinned: &HashSet<usize>,
    text: &dyn Fn(&Value, &DataType) -> String,
    default_lz4: bool,
) -> (usize, Vec<NewExt>, Vec<St>, Vec<bool>) {
    let n = row.len();
    let has_null = row.iter().any(Value::is_null);
    let hoff = heapsim::maxalign(23 + if has_null { n.div_ceil(8) } else { 0 });
    let storage: Vec<char> = t.columns[..n]
        .iter()
        .map(|c| {
            c.storage
                .unwrap_or_else(|| crate::catalog::system::typstorage(&c.ty))
        })
        .collect();
    let has_toast = t.toast != 0;
    let mut st = vec![St::Inline; n];
    let mut ignore = vec![false; n];
    let mut incompressible = vec![false; n];
    // tai_size: the in-memory varlena's size.
    let mut size = vec![0usize; n];
    for i in 0..n {
        if row[i].is_null() || relsize::typlen(&types[i]) != -1 || storage[i] == 'p' {
            ignore[i] = true;
            continue;
        }
        if pinned.contains(&i) {
            st[i] = St::External;
            ignore[i] = true;
            continue;
        }
        let db = relsize::datum_bytes(0, &row[i], &types[i], text);
        size[i] = if db <= 127 { db - 1 } else { db - 4 } + 4;
    }
    let data = |st: &[St]| {
        let mut off = 0;
        for i in 0..n {
            if row[i].is_null() {
                continue;
            }
            match st[i] {
                St::Inline => off += relsize::datum_bytes(off, &row[i], &types[i], text),
                St::Compressed(sz) => {
                    let al = relsize::type_layout(&types[i]).1;
                    off = off.div_ceil(al) * al + sz;
                }
                St::External => off += TOAST_POINTER,
            }
        }
        off
    };
    let biggest = |st: &[St], ignore: &[bool], incompressible: &[bool], size: &[usize], compress: bool, main: bool| {
        let mut best = None;
        let mut bsize = heapsim::maxalign(TOAST_POINTER);
        for i in 0..n {
            if ignore[i] || (compress && incompressible[i]) || st[i] == St::External {
                continue;
            }
            if compress && matches!(st[i], St::Compressed(_)) {
                continue;
            }
            if main != (storage[i] == 'm') || (!main && storage[i] != 'x' && storage[i] != 'e') {
                continue;
            }
            if size[i] > bsize {
                best = Some(i);
                bsize = size[i];
            }
        }
        best
    };
    let try_compress = |i: usize, st: &mut [St], incompressible: &mut [bool], size: &mut [usize]| {
        let valsize = size[i] - 4;
        // The method the value was compressed with, else the column's, else
        // default_toast_compression's.
        let lz4 = match compressed_with(&row[i]).or(t.columns[i].compression) {
            Some(c) => c == 'l',
            None => default_lz4,
        };
        let compress = |b: Vec<u8>| {
            if lz4 {
                crate::catalog::lz4::compressed_len(&b).filter(|&c| c <= b.len())
            } else {
                crate::catalog::pglz::compressed_len(&b)
            }
        };
        match raw_bytes(&row[i], &types[i]).and_then(compress) {
            Some(c) if c + 8 + 2 < valsize => {
                st[i] = St::Compressed(c + 8);
                size[i] = c + 8;
            }
            _ => incompressible[i] = true,
        }
    };
    let mut exts = Vec::new();
    let mut zipped = vec![false; n];
    let mut externalize = |i: usize, st: &mut [St], ignore: &mut [bool], size: &[usize]| {
        ignore[i] = true;
        zipped[i] = matches!(st[i], St::Compressed(_));
        let bytes = match st[i] {
            St::Compressed(sz) => sz - 4,
            _ => size[i] - 4,
        };
        exts.push(NewExt {
            col: i,
            bytes,
            sig: value_sig(&row[i]),
        });
        st[i] = St::External;
    };
    let target = option(&t.options, "toast_tuple_target")
        .and_then(|v| v.parse().ok())
        .unwrap_or(TOAST_THRESHOLD);
    let mut max = target.saturating_sub(hoff);
    while data(&st) > max {
        let Some(i) = biggest(&st, &ignore, &incompressible, &size, true, false) else {
            break;
        };
        if storage[i] == 'x' {
            try_compress(i, &mut st, &mut incompressible, &mut size);
        } else {
            incompressible[i] = true;
        }
        if size[i] > max && has_toast {
            externalize(i, &mut st, &mut ignore, &size);
        }
    }
    while has_toast && data(&st) > max {
        let Some(i) = biggest(&st, &ignore, &incompressible, &size, false, false) else {
            break;
        };
        externalize(i, &mut st, &mut ignore, &size);
    }
    while data(&st) > max {
        let Some(i) = biggest(&st, &ignore, &incompressible, &size, true, true) else {
            break;
        };
        try_compress(i, &mut st, &mut incompressible, &mut size);
    }
    max = TOAST_TARGET_MAIN - hoff;
    while has_toast && data(&st) > max {
        let Some(i) = biggest(&st, &ignore, &incompressible, &size, false, true) else {
            break;
        };
        externalize(i, &mut st, &mut ignore, &size);
    }
    let len = hoff + data(&st);
    (len, exts, st, zipped)
}

/// How a column's value of `row` is stored in `t` (pg_column_size / pg_column_compression):
/// None when it is stored as it is, else its stored size (a compressed varlena's, or an
/// out-of-line value's extsize) and its compression method when compressed.
pub fn stored_datum(
    t: &Table,
    row: &[Value],
    col: usize,
    ctx: &ExecCtx,
) -> Option<(usize, Option<char>)> {
    let n = row.len().min(t.columns.len());
    if col >= n {
        return None;
    }
    let types: Vec<DataType> = t.columns[..n].iter().map(|c| c.ty.clone()).collect();
    let fmt = ctx.fmt();
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
    let row: Vec<Value> = t.columns[..n]
        .iter()
        .zip(row)
        .map(|(c, v)| if c.virtual_gen || c.dropped { Value::Null } else { v.clone() })
        .collect();
    if relsize::heap_tuple_len(&types, &row, &text) <= TOAST_THRESHOLD {
        return None;
    }
    let lz4 = ctx
        .sess
        .settings
        .borrow()
        .get_raw("default_toast_compression")
        .is_some_and(|v| v == "lz4");
    let (_, exts, st, zipped) = toast_layout_full(t, &types, &row, &HashSet::new(), &text, lz4);
    // (The row version it was read from knows best.)
    let known = compressed_with(&row[col]).or_else(|| stored_with(t.oid, col, &row[col]));
    let method = || match known.or(t.columns[col].compression) {
        Some(c) => c,
        None if lz4 => 'l',
        None => 'p',
    };
    match st[col] {
        // A compressed datum copied here stays compressed.
        St::Inline if known.is_some() && t.columns[col].storage != Some('p') => {
            let raw = raw_bytes(&row[col], &types[col])?;
            let len = if known == Some('l') {
                crate::catalog::lz4::compressed_len(&raw)?
            } else {
                crate::catalog::pglz::compressed_len(&raw)?
            };
            Some((len + 8, known))
        }
        St::Inline => None,
        St::Compressed(sz) => Some((sz, Some(method()))),
        St::External => {
            let e = exts.iter().find(|e| e.col == col)?;
            Some((e.bytes, zipped[col].then(method)))
        }
    }
}

/// toast_save_datum: a value's chunks go into the TOAST table and its index.
fn save_toast(
    heaps: &mut HashMap<u32, Heap>,
    btrees: &mut HashMap<u32, Btree>,
    toast: &mut Toast,
    xids: &Xids,
    toast_oid: u32,
    exts: Vec<NewExt>,
    xid: u64,
    use_fsm: bool,
) -> Vec<Ext> {
    let mut out = Vec::with_capacity(exts.len());
    for e in exts {
        toast.next_id += 1;
        let x = Ext {
            col: e.col,
            id: toast.next_id,
            bytes: e.bytes,
            sig: e.sig,
        };
        for seq in 0..x.chunks() {
            let heap = heaps.entry(toast_oid).or_insert_with(|| Heap::new(100));
            let mut mode = InsertMode {
                use_fsm,
                bistate: None,
                frozen: false,
            };
            let tid = heap.insert(toast_key(x.id, seq), chunk_len(x.bytes, seq), xid, &mut mode);
            btrees
                .entry(toast_oid + 1)
                .or_insert_with(|| Btree::build(toast_index_opts(), Vec::new()));
            let heap = &heaps[&toast_oid];
            index_insert(
                btrees,
                heap,
                xids,
                vec![(toast_oid + 1, toast_index_key(x.id, seq), false)],
                tid,
            );
        }
        out.push(x);
    }
    out
}

/// toast_delete_datum: a value's chunks are deleted.
fn delete_toast(heaps: &mut HashMap<u32, Heap>, toast_oid: u32, exts: &[Ext], xid: u64, v: &Vis) {
    let Some(heap) = heaps.get_mut(&toast_oid) else {
        return;
    };
    for e in exts {
        for seq in 0..e.chunks() {
            heap.delete(&toast_key(e.id, seq), xid, v);
        }
    }
}

/// A TOAST table written afresh (a rewrite): chunks in row order, its index built.
fn rebuild_toast(rf: &mut RelFiles, t: &Table, rows: Vec<(Vec<u8>, Vec<NewExt>)>) {
    if t.toast == 0 {
        return;
    }
    let mut toast = Toast::default();
    let mut chunks = Vec::new();
    for (k, exts) in rows {
        let mut saved = Vec::with_capacity(exts.len());
        for e in exts {
            toast.next_id += 1;
            let x = Ext {
                col: e.col,
                id: toast.next_id,
                bytes: e.bytes,
                sig: e.sig,
            };
            for seq in 0..x.chunks() {
                chunks.push((toast_key(x.id, seq), chunk_len(x.bytes, seq), x.id, seq));
            }
            saved.push(x);
        }
        if !saved.is_empty() {
            toast.rows.insert(k, saved);
        }
    }
    let ids: Vec<(u64, u32)> = chunks.iter().map(|c| (c.2, c.3)).collect();
    let (h, tids) = heapsim::rewritten(100, chunks.into_iter().map(|c| (c.0, c.1)).collect());
    let ents = ids
        .into_iter()
        .zip(tids)
        .map(|((id, seq), tid)| (toast_index_key(id, seq), tid))
        .collect();
    rf.heaps.insert(t.toast, h);
    rf.btrees
        .insert(t.toast + 1, Btree::build(toast_index_opts(), ents));
    rf.toast.insert(t.oid, toast);
}

/// The btree entry of `row` for index `bix`, given its key values.
fn index_key(
    t: &Table,
    bix: &BoundIndex,
    mut vals: Vec<Value>,
    row: &[Value],
    ctx: &ExecCtx,
) -> Arc<Key> {
    // (name_ops keeps a name key as a cstring: opckeytype.)
    let mut types: Vec<DataType> = bix
        .keys
        .iter()
        .map(|k| match k.ty() {
            DataType::Name => DataType::Cstring,
            t => t,
        })
        .collect();
    for &i in &bix.index.include {
        types.push(t.columns.get(i).map_or(DataType::Text, |c| c.ty.clone()));
        vals.push(row.get(i).cloned().unwrap_or(Value::Null));
    }
    let fmt = ctx.fmt();
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
    let mut atts = Vec::with_capacity(vals.len());
    let mut sizes = Vec::with_capacity(vals.len());
    for (n, v) in vals.iter().enumerate() {
        let mut b = Vec::new();
        let desc = bix.index.columns.get(n).is_some_and(|c| c.desc);
        let nulls_first = bix.index.columns.get(n).is_some_and(|c| c.nulls_first);
        crate::storage::keys::encode_index_value(&mut b, v, desc, nulls_first);
        atts.push(b);
        sizes.push(relsize::index_tuple(&types[..=n], &vals[..=n], &text));
    }
    Arc::new(Key::new(&atts, &sizes))
}

/// `row`'s entries in the table's btree indexes.
fn entries(
    t: &Table,
    indexes: &[BoundIndex],
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<Vec<(u32, Arc<Key>)>> {
    let mut out = Vec::new();
    for bix in indexes {
        if btree_opts(bix).is_none() {
            continue;
        }
        if let Some(vals) = index_values(bix, row, ctx)? {
            out.push((bix.index.oid, index_key(t, bix, vals, row, ctx)));
        }
    }
    Ok(out)
}

/// Make sure the table's heap and btree indexes are modelled, laying out those that
/// aren't from the rows there now. False when the table is too big to model.
fn ensure(t: &Table, indexes: &[BoundIndex], ctx: &ExecCtx) -> Result<bool> {
    let db = &ctx.sess.db;
    let (heap_missing, missing): (bool, Vec<&BoundIndex>) = {
        let rf = db.relfiles.lock();
        if rf.untracked.contains(&t.oid) {
            return Ok(false);
        }
        let missing = indexes
            .iter()
            .filter(|b| btree_opts(b).is_some() && !rf.btrees.contains_key(&b.index.oid))
            .collect();
        (!rf.heaps.contains_key(&t.oid), missing)
    };
    if !heap_missing && missing.is_empty() {
        return Ok(true);
    }
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    // (Read up to the limit; a bigger table's rest is only measured, a row at a time.)
    let mut raw: Vec<(Vec<u8>, bytes::Bytes)> = Vec::new();
    let mut est = None;
    {
        // (The scan holds no borrow of the transaction: `layout` may take one.)
        let mut it = ctx.txn.borrow().scan(&start, &end);
        for r in it.by_ref() {
            raw.push(r?);
            if raw.len() > MAX_VERSIONS {
                break;
            }
        }
        if raw.len() > MAX_VERSIONS {
            // The rows read so far are laid out as stored; the rest (a big table's, read
            // when its database opens) by their stored length, scaled as the first rows'
            // tuples are to theirs: no decoding, nothing held.
            let mut e = EstHeap::default();
            let ff = heap_fillfactor(t);
            let (mut tuple, mut stored) = (0usize, 0usize);
            for (_, v) in raw.drain(..) {
                let len = layout(t, &decode_table_row(t, &v)?, ctx, &HashSet::new()).0;
                tuple += len;
                stored += v.len();
                e.add(len, ff);
            }
            let scale = tuple as f64 / stored.max(1) as f64;
            for r in it {
                let (_, v) = r?;
                e.add((v.len() as f64 * scale).round() as usize, ff);
            }
            est = Some(e);
        }
    }
    if let Some(est) = est {
        untrack(db, t, Some(est));
        return Ok(false);
    }
    // Rows go back in the order of the heap TIDs they were written with (those without
    // one after them, in storage order), so a heap laid out afresh keeps their order.
    let mut raw = raw;
    let mut stored: Vec<Option<Tid>> = Vec::new();
    if heap_missing {
        let mut tagged: Vec<(Option<Tid>, usize, (Vec<u8>, bytes::Bytes))> = raw
            .into_iter()
            .enumerate()
            .map(|(i, kv)| (super::scan::stored_tid(&kv.1), i, kv))
            .collect();
        tagged.sort_by(|a, b| match (a.0, b.0) {
            (Some(x), Some(y)) => x.cmp(&y).then(a.1.cmp(&b.1)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        stored = tagged.iter().map(|x| x.0).collect();
        raw = tagged.into_iter().map(|x| x.2).collect();
    }
    let mut rows = Vec::with_capacity(raw.len());
    for (k, v) in raw {
        rows.push((k, decode_table_row(t, &v)?));
    }
    let mut lens = Vec::new();
    let mut exts = Vec::new();
    if heap_missing {
        for (k, r) in &rows {
            let (len, e) = layout(t, r, ctx, &HashSet::new());
            lens.push((k.clone(), len));
            exts.push((k.clone(), e));
        }
    }
    let mut built = Vec::new();
    for bix in missing {
        let mut ents = Vec::new();
        for (k, r) in &rows {
            if let Some(vals) = index_values(bix, r, ctx)? {
                ents.push((index_key(t, bix, vals, r, ctx), k));
            }
        }
        built.push((bix.index.oid, btree_opts(bix).unwrap_or_else(default_opts), ents));
    }
    let mut rf = db.relfiles.lock();
    let rf = &mut *rf;
    if heap_missing && exts.iter().any(|(_, e)| !e.is_empty()) {
        mark_tracked(db.instance, t.oid);
    }
    if heap_missing && !rf.heaps.contains_key(&t.oid) {
        let (mut h, tids) = heapsim::rewritten(heap_fillfactor(t), lens);
        h.unstamped = stored.iter().zip(&tids).any(|(s, t)| s.is_some_and(|s| s != *t));
        rf.heaps.insert(t.oid, h);
        rebuild_toast(rf, t, exts);
    }
    let heap = &rf.heaps[&t.oid];
    for (oid, opts, ents) in built {
        if rf.btrees.contains_key(&oid) {
            continue;
        }
        let ents: Vec<(Arc<Key>, Tid)> = ents
            .into_iter()
            .filter_map(|(key, k)| heap.tid_of(k).map(|tid| (key, tid)))
            .collect();
        rf.btrees.insert(oid, Btree::build(opts, ents));
    }
    Ok(true)
}

fn default_opts() -> BtreeOpts {
    BtreeOpts {
        nkey: 1,
        unique: false,
        dedup: false,
        fillfactor: 90,
    }
}

/// Forget a table's files (`keep_untracked`: unless it is too big to model).
fn forget(rf: &mut RelFiles, t: &Table) {
    rf.heaps.remove(&t.oid);
    for i in t.indexes.iter().chain(&t.pkey_index) {
        rf.btrees.remove(i);
    }
    if t.toast != 0 {
        rf.heaps.remove(&t.toast);
        rf.btrees.remove(&(t.toast + 1));
    }
    rf.toast.remove(&t.oid);
}

/// Stop modelling a table that grew too big; its size is estimated from here on (from its
/// heap model's, or `est`).
fn untrack(db: &Database, t: &Table, est: Option<EstHeap>) {
    let mut rf = db.relfiles.lock();
    let est = est
        .or_else(|| rf.heaps.get(&t.oid).map(EstHeap::from_heap))
        .unwrap_or_default();
    rf.untracked.insert(t.oid);
    forget(&mut rf, t);
    rf.estimated.insert(t.oid, est);
}

fn check_size(db: &Database, t: &Table) {
    let big = db
        .relfiles
        .lock()
        .heaps
        .get(&t.oid)
        .is_some_and(|h| h.versions() > MAX_VERSIONS);
    if big {
        untrack(db, t, None);
    }
}

/// The pruning horizon for `table` (GlobalVisTestFor): a temporary table's (or its TOAST
/// table's) is its session's own transaction, everything else's the running transactions'
/// and held snapshots'.
fn vis_for<'a>(xids: &'a Xids, table: u32, ctx: &ExecCtx) -> Vis<'a> {
    let cat = ctx.catalog();
    let rel = cat.tables.get(&table).or_else(|| cat.toast_owner(table).map(|(o, _)| o));
    if rel.is_some_and(|t| cat.schema_name(t.schema).starts_with("pg_temp")) {
        Vis::temp(xids, ctx.txn.borrow().xid_if_assigned())
    } else {
        Vis::new(xids)
    }
}

/// Add a btree entry for a new heap tuple at `tid`.
fn index_insert(
    btrees: &mut HashMap<u32, Btree>,
    heap: &Heap,
    xids: &Xids,
    entries: Vec<(u32, Arc<Key>, bool)>,
    tid: Tid,
) {
    let v = Vis::new(xids);
    let dead = |t: Tid| heap.is_dead(t, &v);
    for (oid, key, unchanged) in entries {
        if let Some(bt) = btrees.get_mut(&oid) {
            bt.insert(
                key,
                tid,
                &InsertHint {
                    unchanged,
                    dead: &dead,
                },
            );
        }
    }
}

/// A row is about to be inserted (heap_insert and its index entries).
pub fn inserted(
    t: &Table,
    indexes: &[BoundIndex],
    key: &[u8],
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<Option<Tid>> {
    note_compression(t, row, Some(key), ctx);
    if !ensure(t, indexes, ctx)? {
        estimate_grows(t, row, ctx);
        return Ok(None);
    }
    let (len, exts) = layout(t, row, ctx, &HashSet::new());
    let entries = entries(t, indexes, row, ctx)?;
    let xid = ctx.txn.borrow_mut().cur_xid();
    let cid = ctx.txn.borrow_mut().output_cid();
    let txn = ctx.txn.borrow().id;
    let db = &ctx.sess.db;
    let xids = db.xids();
    let tid = {
        let mut rf = db.relfiles.lock();
        let RelFiles {
            heaps,
            btrees,
            bulk,
            toast,
            ..
        } = &mut *rf;
        if !heaps.contains_key(&t.oid) {
            return Ok(None);
        }
        let b = bulk.get_mut(&(t.oid, txn));
        if let Some(Bulk {
            batch: Some(batch), ..
        }) = b
        {
            batch.rows.push(Pending {
                key: key.to_vec(),
                len,
                exts,
                entries,
            });
            return Ok(None);
        }
        let mut single = InsertMode::single();
        let mode = match b {
            Some(b) => &mut b.mode,
            None => &mut single,
        };
        if !exts.is_empty() {
            let tt = toast.entry(t.oid).or_default();
            let saved = save_toast(heaps, btrees, tt, &xids, t.toast, exts, xid, mode.use_fsm);
            tt.rows.insert(key.to_vec(), saved);
        }
        let Some(heap) = heaps.get_mut(&t.oid) else {
            return Ok(None);
        };
        let tid = heap.insert(key.to_vec(), len, xid, mode);
        heap.set_raw_cid(tid, cid);
        let entries = entries.into_iter().map(|(o, k)| (o, k, false)).collect();
        index_insert(btrees, heap, &xids, entries, tid);
        tid
    };
    check_size(db, t);
    Ok(Some(tid))
}

/// The columns an index's entries depend on.
fn index_columns(bix: &BoundIndex, out: &mut HashSet<usize>) {
    let mut add = |e: &Expr| {
        e.walk(&mut |x| {
            if let Expr::Col(i, _) = x {
                out.insert(*i);
            }
        })
    };
    for k in &bix.keys {
        add(k);
    }
    if let Some(p) = &bix.predicate {
        add(p);
    }
    out.extend(bix.index.include.iter().copied());
}

fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    let enc = |v: Option<&Value>| {
        let mut out = Vec::new();
        encode_value(&mut out, v.unwrap_or(&Value::Null), false);
        out
    };
    enc(a) == enc(b)
}

fn same_vals(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(Some(x), Some(y)))
}

/// A row is about to get a new version (heap_update: a HOT update when no indexed column
/// changed and the new version fits on the old one's page; otherwise new index entries).
pub fn updated(
    t: &Table,
    indexes: &[BoundIndex],
    old_key: &[u8],
    old: &[Value],
    new_key: &[u8],
    new: &[Value],
    ctx: &ExecCtx,
) -> Result<Option<(Tid, bool)>> {
    note_compression(t, new, Some(new_key), ctx);
    if !ensure(t, indexes, ctx)? {
        estimate_grows(t, new, ctx);
        return Ok(None);
    }
    let db = &ctx.sess.db;
    // Out-of-line values the new version keeps as they are.
    let old_exts: Vec<Ext> = (db.relfiles.lock().toast.get(&t.oid))
        .and_then(|tt| tt.rows.get(old_key).cloned())
        .unwrap_or_default();
    let pinned: HashSet<usize> = old_exts
        .iter()
        .filter(|e| new.get(e.col).is_some_and(|v| value_sig(v) == e.sig))
        .map(|e| e.col)
        .collect();
    let (len, exts) = layout(t, new, ctx, &pinned);
    // (A summarizing index, BRIN, doesn't keep an update from being HOT.)
    let mut cols = HashSet::new();
    for bix in indexes.iter().filter(|b| b.index.method != "brin") {
        index_columns(bix, &mut cols);
    }
    let hot_ok = old_key == new_key && cols.iter().all(|&c| same(old.get(c), new.get(c)));
    let xid = ctx.txn.borrow_mut().cur_xid();
    let xids = db.xids();
    let (tid, hot) = {
        let mut rf = db.relfiles.lock();
        let RelFiles {
            heaps,
            btrees,
            toast,
            ..
        } = &mut *rf;
        if !heaps.contains_key(&t.oid) {
            return Ok(None);
        }
        let v = vis_for(&xids, t.oid, ctx);
        if !old_exts.is_empty() || !exts.is_empty() {
            let tt = toast.entry(t.oid).or_default();
            let mut keep: Vec<Ext> = Vec::new();
            let mut gone: Vec<Ext> = Vec::new();
            for e in old_exts {
                if pinned.contains(&e.col) {
                    keep.push(e);
                } else {
                    gone.push(e);
                }
            }
            keep.extend(save_toast(heaps, btrees, tt, &xids, t.toast, exts, xid, true));
            delete_toast(heaps, t.toast, &gone, xid, &v);
            if old_key != new_key {
                tt.rows.remove(old_key);
            }
            if keep.is_empty() {
                tt.rows.remove(new_key);
            } else {
                tt.rows.insert(new_key.to_vec(), keep);
            }
        }
        let Some(heap) = heaps.get_mut(&t.oid) else {
            return Ok(None);
        };
        mark_cmax(heap, old_key, &v, ctx);
        let cid = ctx.txn.borrow_mut().output_cid();
        let r = heap.update(old_key, new_key.to_vec(), len, xid, hot_ok, &v);
        heap.set_raw_cid(r.0, cid);
        r
    };
    if !hot {
        let mut ents = Vec::new();
        for bix in indexes {
            if btree_opts(bix).is_none() {
                continue;
            }
            let Some(nv) = index_values(bix, new, ctx)? else {
                continue;
            };
            let unchanged = index_values(bix, old, ctx)?.is_some_and(|ov| same_vals(&ov, &nv));
            ents.push((bix.index.oid, index_key(t, bix, nv, new, ctx), unchanged));
        }
        let mut rf = db.relfiles.lock();
        let RelFiles { heaps, btrees, .. } = &mut *rf;
        if let Some(heap) = heaps.get(&t.oid) {
            index_insert(btrees, heap, &xids, ents, tid);
        }
    }
    check_size(db, t);
    Ok(Some((tid, hot)))
}

/// The live version of `key` is being deleted by the running command: its raw command id
/// becomes the deleting command's, or a combo command id when this transaction inserted
/// it (HeapTupleHeaderAdjustCmax).
fn mark_cmax(heap: &mut Heap, key: &[u8], v: &Vis, ctx: &ExecCtx) {
    let Some(tid) = heap.locate(key, v) else {
        return;
    };
    let Some(t) = heap.tuple(tid) else {
        return;
    };
    let (xmin, cmin) = (t.xmin, t.cid);
    let Ok(mut txn) = ctx.txn.try_borrow_mut() else {
        return;
    };
    let here = txn.is_current_xid(xmin);
    let c = txn.adjust_cmax(cmin, here);
    heap.set_raw_cid(tid, c);
}

/// The raw command id (cmin / cmax) of the tuple at `tid` of `table`, when modelled.
pub fn raw_cid(table: u32, tid: Tid, db: &Database) -> Option<u32> {
    let rf = db.relfiles.lock();
    rf.heaps.get(&table)?.tuple(tid).map(|t| t.cid)
}

/// A row is about to be deleted (heap_delete, and its out-of-line values).
pub fn deleted(t: &Table, indexes: &[BoundIndex], key: &[u8], ctx: &ExecCtx) -> Result<()> {
    if !ensure(t, indexes, ctx)? {
        return Ok(());
    }
    let xid = ctx.txn.borrow_mut().cur_xid();
    let db = &ctx.sess.db;
    let xids = db.xids();
    let v = vis_for(&xids, t.oid, ctx);
    let mut rf = db.relfiles.lock();
    let RelFiles { heaps, toast, .. } = &mut *rf;
    if let Some(heap) = heaps.get_mut(&t.oid) {
        mark_cmax(heap, key, &v, ctx);
        heap.delete(key, xid, &v);
    }
    if let Some(exts) = toast.get(&t.oid).and_then(|tt| tt.rows.get(key)) {
        delete_toast(heaps, t.toast, exts, xid, &v);
    }
    Ok(())
}

/// A sequential scan of the table starts: every page it reads may be pruned.
pub fn scanned(table: u32, ctx: &ExecCtx) {
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    if let Some(heap) = rf.heaps.get_mut(&table) {
        heap.prune_all(&vis_for(&xids, table, ctx));
    }
}

/// A sequential scan reads pages `from..to` (heapgetpage): each may be pruned as it is
/// read. None for `to`: up to the end.
pub fn scanned_pages(table: u32, from: u32, to: Option<u32>, ctx: &ExecCtx) {
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    if let Some(heap) = rf.heaps.get_mut(&table) {
        let v = vis_for(&xids, table, ctx);
        let end = to.unwrap_or(heap.nblocks()).min(heap.nblocks());
        for b in from..end {
            heap.prune_opt(b, &v);
        }
    }
}

/// An index or bitmap heap scan fetches a row of `table` (by data key): the page it is on
/// may be pruned when the scan moves to it (heapam_index_fetch_tuple). Returns the page,
/// so the caller can tell when it moves (None: not modelled).
pub fn fetched(table: u32, key: &[u8], last: Option<u32>, ctx: &ExecCtx) -> Option<u32> {
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    let heap = rf.heaps.get_mut(&table)?;
    let v = vis_for(&xids, table, ctx);
    let blk = heap.locate(key, &v)?.0;
    if last != Some(blk) {
        heap.prune_opt(blk, &v);
    }
    Some(blk)
}

/// An index scan of btree `index` over storage key ranges `ranges` (each key starting
/// with `prefix`, the index's): the entries it passes whose heap tuples are all dead get
/// LP_DEAD, for a later insertion's simple deletion to reclaim. With `entries`, the
/// entries the scan returns (those not LP_DEAD before it), in index order: each heap TID
/// and whether its page isn't all-visible (an index-only scan's heap fetch); None when
/// the index isn't modelled.
pub fn index_scanned(
    table: u32,
    index: u32,
    prefix: &[u8],
    ranges: &[(Vec<u8>, Vec<u8>)],
    entries: bool,
    ctx: &ExecCtx,
) -> Option<Vec<(Tid, bool)>> {
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    let rf = &mut *rf;
    let (Some(heap), Some(bt)) = (rf.heaps.get(&table), rf.btrees.get_mut(&index)) else {
        return None;
    };
    let v = vis_for(&xids, table, ctx);
    let all_dead = |t: Tid| heap.chain_dead(t, &v);
    let mut seen = entries.then(Vec::new);
    for (lo, hi) in ranges {
        let lo = lo.strip_prefix(prefix);
        let hi = hi.strip_prefix(prefix);
        bt.kill(lo, hi, &all_dead, seen.as_mut());
    }
    seen.map(|s| s.into_iter().map(|t| (t, !heap.page_all_visible(t.0))).collect())
}

/// What gave relation `rel` new storage (TRUNCATE, a rewrite) rolled back: its old files
/// are in use again, laid out afresh from its rows (in their heap order) when next needed.
pub fn storage_rolled_back(rel: u32, db: &Database) {
    let cat = db.catalog();
    let mut rf = db.relfiles.lock();
    match cat.tables.get(&rel) {
        Some(t) => forget(&mut rf, t),
        None => {
            rf.heaps.remove(&rel);
        }
    }
}

/// An AFTER ROW trigger of `table` runs with the row (data key `key`) in a tuple slot, and
/// an UPDATE's with the old version too (`old_too`): each keeps its page pinned, so a scan
/// of the table the trigger makes can't prune those pages. Returns the pages pinned, for
/// `trigger_unpin`.
pub fn trigger_pin(table: u32, key: &[u8], old_too: bool, ctx: &ExecCtx) -> Vec<u32> {
    if key.is_empty() {
        return Vec::new();
    }
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    let Some(heap) = rf.heaps.get_mut(&table) else {
        return Vec::new();
    };
    let Some((tid, prev)) = heap.locate_with_prev(key, &Vis::new(&xids)) else {
        return Vec::new();
    };
    let mut pins = vec![tid.0];
    if old_too && let Some(p) = prev {
        pins.push(p.0);
    }
    for b in &pins {
        heap.pin(*b, true);
    }
    pins
}

/// The trigger is done with its rows: their pages' pins go.
pub fn trigger_unpin(table: u32, pins: &[u32], ctx: &ExecCtx) {
    if pins.is_empty() {
        return;
    }
    let mut rf = ctx.sess.db.relfiles.lock();
    if let Some(heap) = rf.heaps.get_mut(&table) {
        for b in pins {
            heap.pin(*b, false);
        }
    }
}

/// pg_column_toast_chunk_id: the chunk_id of `v`'s out-of-line copy, when it is a datum
/// read from a table that stores it in its TOAST table.
pub fn toast_chunk_id(v: &Value, ctx: &ExecCtx) -> Option<u32> {
    let (t, col, (_, key)) = read_from(v)?;
    let key = key?;
    let rf = ctx.sess.db.relfiles.lock();
    let ext = rf.toast.get(&t)?.rows.get(&key[..])?.iter().find(|e| e.col == col)?;
    Some(ext.id as u32)
}

/// The rows of table `t`'s TOAST table (chunk_id, chunk_seq, chunk_data), in chunk order:
/// each out-of-line value of its rows cut into TOAST_MAX_CHUNK_SIZE pieces.
pub fn toast_chunks(t: &Table, ctx: &ExecCtx) -> Result<Vec<(u32, i32, Vec<u8>)>> {
    let indexes = crate::sql::binder::bound_indexes(&Arc::new(t.clone()), ctx)?;
    if !ensure(t, &indexes, ctx)? {
        return Ok(Vec::new());
    }
    let rows: Vec<(Vec<u8>, Vec<Ext>)> = {
        let rf = ctx.sess.db.relfiles.lock();
        rf.toast.get(&t.oid).map_or_else(Vec::new, |tt| {
            tt.rows.iter().map(|(k, e)| (k.clone(), e.clone())).collect()
        })
    };
    let mut out = Vec::new();
    for (key, exts) in rows {
        let latest = ctx.txn.borrow().get(&key)?;
        let Some(bytes) = latest else {
            continue;
        };
        let row = decode_table_row(t, &bytes)?;
        for e in exts {
            let ty = t.columns.get(e.col).map_or(DataType::Text, |c| c.ty.clone());
            let mut data = row
                .get(e.col)
                .and_then(|v| raw_bytes(v, &ty))
                .unwrap_or_default();
            data.resize(e.bytes, 0);
            for (seq, chunk) in data.chunks(TOAST_CHUNK).enumerate() {
                out.push((e.id as u32, seq as i32, chunk.to_vec()));
            }
        }
    }
    out.sort_by_key(|c| (c.0, c.1));
    Ok(out)
}

/// The table's size in pages as modelled (None: not modelled).
pub fn modelled_blocks(table: u32, ctx: &ExecCtx) -> Option<u32> {
    ctx.sess.db.relfiles.lock().heaps.get(&table).map(Heap::nblocks)
}

/// The table was emptied (TRUNCATE): new, empty files.
pub fn truncated(t: &Table, db: &Database) {
    let mut rf = db.relfiles.lock();
    rf.untracked.remove(&t.oid);
    rf.estimated.remove(&t.oid);
    forget(&mut rf, t);
    rf.heaps.insert(t.oid, Heap::new(heap_fillfactor(t)));
}

/// TRUNCATE builds the table's indexes anew over the empty heap: a hash index starts with
/// the buckets estimate_rel_size's guess for the new, empty files gives.
pub fn indexes_emptied(t: &Table, ctx: &ExecCtx) {
    let est = crate::sql::costs::empty_rel_tuples(t, ctx);
    let mut rf = ctx.sess.db.relfiles.lock();
    for ix in &t.indexes {
        rf.hash_est.insert(*ix, est);
        rf.gin_built.insert(*ix, vec![HashSet::new()]);
        rf.brin.insert(*ix, BrinRanges::default());
    }
}

/// A GIN index's entries for one value (ginExtractEntries) under its operator class: None
/// for one this model doesn't know.
fn gin_keys(v: &Value, ty: &DataType, opclass: u32) -> Option<Vec<crate::catalog::ginsim::GinKey>> {
    use crate::catalog::ginsim::GinKey;
    let text_key = |b: Vec<u8>| {
        let n = b.len();
        GinKey {
            category: 0,
            datum: if n <= 126 { 1 + n } else { 4 + n },
            sort: b,
        }
    };
    let null = |c: u8| GinKey {
        category: c,
        sort: Vec::new(),
        datum: 0,
    };
    if v.is_null() {
        // GIN_CAT_NULL_ITEM.
        return Some(vec![null(3)]);
    }
    let mut keys: Vec<GinKey> = match (v, opclass) {
        (Value::TsVector(tv), _) => tv
            .lexemes
            .iter()
            .map(|l| text_key(l.word.as_bytes().to_vec()))
            .collect(),
        (Value::Jsonb(j), 0 | 10090) => {
            fn scalar(j: &crate::types::json::Jsonb, key: bool) -> Option<(u8, String)> {
                use crate::types::json::Jsonb as J;
                Some(match j {
                    J::Null => (0x02, String::new()),
                    J::Bool(b) => (0x03, if *b { "t" } else { "f" }.into()),
                    J::Number(n) => (0x04, n.normalize().to_string()),
                    J::String(s) => (if key { 0x01 } else { 0x05 }, s.clone()),
                    _ => return None,
                })
            }
            fn walk(j: &crate::types::json::Jsonb, out: &mut Vec<(u8, String)>) {
                use crate::types::json::Jsonb as J;
                match j {
                    J::Object(pairs) => {
                        for (k, v) in pairs {
                            out.push((0x01, k.clone()));
                            match v {
                                J::Object(_) | J::Array(_) => walk(v, out),
                                v => out.extend(scalar(v, false)),
                            }
                        }
                    }
                    J::Array(items) => {
                        for v in items {
                            match v {
                                J::Object(_) | J::Array(_) => walk(v, out),
                                // (String array elements are taken as keys.)
                                v => out.extend(scalar(v, matches!(v, J::String(_)))),
                            }
                        }
                    }
                    v => out.extend(scalar(v, false)),
                }
            }
            let mut raw = Vec::new();
            walk(j, &mut raw);
            raw.into_iter()
                .map(|(flag, s)| {
                    let mut b = vec![flag];
                    if s.len() > 125 {
                        // JGINFLAG_HASHED with the hash in hex.
                        b[0] |= 0x10;
                        let h = crate::catalog::hashidx::hash_bytes(s.as_bytes());
                        b.extend_from_slice(format!("{h:08x}").as_bytes());
                    } else {
                        b.extend_from_slice(s.as_bytes());
                    }
                    text_key(b)
                })
                .collect()
        }
        (Value::Array(a), 0 | 10064) => {
            let mut out = Vec::new();
            let mut has_null = false;
            for e in &a.items {
                if e.is_null() {
                    has_null = true;
                    continue;
                }
                let datum = match e {
                    Value::Int2(_) => 2,
                    Value::Int4(_) | Value::Oid(_) | Value::Date(_) | Value::Float4(_) => 4,
                    Value::Int8(_) | Value::Float8(_) | Value::Timestamp(_) | Value::Timestamptz(_) => 8,
                    Value::Bool(_) | Value::Char(_) => 1,
                    Value::Text(s) => {
                        let n = s.len();
                        if n <= 126 { 1 + n } else { 4 + n }
                    }
                    _ => return None,
                };
                let mut sort = Vec::new();
                encode_value(&mut sort, e, false);
                out.push(GinKey {
                    category: 0,
                    sort,
                    datum,
                });
            }
            if has_null {
                // GIN_CAT_NULL_KEY.
                out.push(null(1));
            }
            out
        }
        _ => return None,
    };
    let _ = ty;
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        // GIN_CAT_EMPTY_ITEM.
        keys.push(null(2));
    }
    Some(keys)
}

/// A GIN index's pages: its build over the rows there when it was made, the rows written
/// since in its pending list (None: an operator class or index shape not modelled).
fn gin_index_pages(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    ctx: &ExecCtx,
) -> Result<Option<u64>> {
    Ok(gin_model(t, bix, indexes, ctx)?.map(|g| g.nblocks()))
}

/// The GIN index as its pages hold it now (None: not modelled).
fn gin_model(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    ctx: &ExecCtx,
) -> Result<Option<crate::catalog::ginsim::GinIndex>> {
    use crate::catalog::ginsim::{GinKey, tid_code};
    if bix.keys.len() != 1 || !ensure(t, indexes, ctx)? {
        return Ok(None);
    }
    let fastupdate = option(&bix.index.options, "fastupdate")
        .is_none_or(|v| !matches!(v.to_ascii_lowercase().as_str(), "off" | "false" | "0"));
    let opclass = bix.index.columns.first().map_or(0, |c| c.opclass);
    let ty = bix.keys[0].ty();
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let raw: Vec<(Vec<u8>, bytes::Bytes)> =
        ctx.txn.borrow().scan(&start, &end).collect::<Result<_>>()?;
    // Each row's keys and TID, in heap order.
    let mut rows: Vec<((u32, u16), Vec<u8>, Vec<GinKey>)> = Vec::new();
    {
        let xids = ctx.sess.db.xids();
        let v = Vis::new(&xids);
        let rf = ctx.sess.db.relfiles.lock();
        let Some(heap) = rf.heaps.get(&t.oid) else {
            return Ok(None);
        };
        for (k, bytes) in &raw {
            let row = decode_table_row(t, bytes)?;
            let Some(vals) = index_values(bix, &row, ctx)? else {
                continue;
            };
            let Some(keys) = vals.first().and_then(|val| gin_keys(val, &ty, opclass)) else {
                return Ok(None);
            };
            let Some(tid) = heap.locate(k, &v) else {
                continue;
            };
            rows.push((tid, k.clone(), keys));
        }
    }
    rows.sort_by_key(|r| r.0);
    // The rows the build read, then those each VACUUM since found (its pending list
    // cleanup moved them into the tree).
    let history = ctx
        .sess
        .db
        .relfiles
        .lock()
        .gin_built
        .get(&bix.index.oid)
        .cloned()
        .unwrap_or_default();
    let stage = |k: &Vec<u8>| history.iter().position(|s| s.contains(k)).unwrap_or(history.len());
    let mut g = crate::catalog::ginsim::GinIndex::new();
    let mut entries: std::collections::BTreeMap<GinKey, Vec<u64>> = Default::default();
    let mut later: Vec<Vec<(u64, Vec<GinKey>)>> = vec![Vec::new(); history.len().max(1)];
    for (tid, k, keys) in rows {
        let code = tid_code(tid.0, tid.1);
        match stage(&k) {
            // (No record of the build: every row counts as built.)
            0 => {
                for key in keys {
                    entries.entry(key).or_default().push(code);
                }
            }
            i => later[i - 1].push((code, keys)),
        }
    }
    // The build: keys in order.
    for (key, tids) in entries {
        g.insert(key, tids);
    }
    // Rows written after it: through the pending list (cleaned up by the VACUUMs that
    // saw them), or straight into the tree.
    let n = later.len();
    for (i, batch) in later.into_iter().enumerate() {
        for (code, keys) in batch {
            if fastupdate {
                g.insert_pending(keys, code);
            } else {
                for key in keys {
                    g.insert(key, vec![code]);
                }
            }
        }
        if i + 1 < n && !history.is_empty() {
            g.cleanup();
        }
    }
    Ok(Some(g))
}

/// SSI page ids a GIN index's scan for these query values locks (the metapage 0, and each
/// key's posting tree or else its entry leaf page), or an insertion of a row with these
/// indexed values checks (the metapage with fast update on, else the same per key).
/// None: not modelled.
pub fn gin_ssi_pages(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    vals: &[Value],
    insert: bool,
    ctx: &ExecCtx,
) -> Result<Option<Vec<u32>>> {
    let opclass = bix.index.columns.first().map_or(0, |c| c.opclass);
    let ty = bix.keys.first().map(|k| k.ty());
    let Some(ty) = ty else { return Ok(None) };
    let fastupdate = option(&bix.index.options, "fastupdate")
        .is_none_or(|v| !matches!(v.to_ascii_lowercase().as_str(), "off" | "false" | "0"));
    let Some(g) = gin_model(t, bix, indexes, ctx)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    if insert && fastupdate {
        return Ok(Some(vec![0]));
    }
    if !insert {
        out.push(0);
    }
    for v in vals.iter().filter(|v| !v.is_null()) {
        let Some(keys) = gin_keys(v, &ty, opclass) else {
            return Ok(None);
        };
        // (startScanEntry / ginEntryInsert: a key in a posting tree locks / checks its
        // root alone, any other its entry leaf page.)
        for k in keys {
            let (leaf, tree) = g.key_pages(&k);
            out.push(tree.unwrap_or(leaf));
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(Some(out))
}

/// gin_clean_pending_list: the pending list pages of GIN index `index` on `t` the cleanup
/// frees (its rows go into the tree).
pub fn gin_clean_pending(t: &Arc<Table>, index: u32, ctx: &ExecCtx) -> Result<i64> {
    let indexes = crate::sql::binder::bound_indexes(t, ctx)?;
    let Some(bix) = indexes.iter().find(|b| b.index.oid == index) else {
        return Ok(0);
    };
    let pages = gin_model(t, bix, &indexes, ctx)?.map_or(0, |g| g.pending_pages());
    gin_cleaned(t, Some(index), ctx)?;
    Ok(pages as i64)
}

/// A GiST point_ops or range_ops index's pages (ranges of int4, int8, date and
/// timestamps): the sorted build over the live rows it read, then gistdoinsert of those
/// written since, in heap order. (Postgres breaks ties between equally good subtrees
/// randomly; the model keeps the first.)
fn gist_index_pages(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    ctx: &ExecCtx,
) -> Result<Option<u64>> {
    Ok(gist_model(t, bix, indexes, ctx)?.map(|(g, _)| g.nblocks()))
}

/// A GiST index scan of a modelled point_ops or range_ops index: the data keys of the rows
/// `test` (the scan's consistent and distance functions, see `Gist::search`) returns, in
/// the scan's order (None: the index isn't modelled).
pub fn gist_scan(
    t: &Arc<Table>,
    index: u32,
    orderbys: usize,
    test: &mut dyn FnMut(Option<&crate::catalog::gistsim::Key>, bool) -> Option<Vec<Option<f64>>>,
    ctx: &ExecCtx,
) -> Result<Option<Vec<Vec<u8>>>> {
    Ok(gist_scan_pages(t, index, orderbys, test, false, ctx)?.map(|r| r.0))
}

/// `gist_scan`, with the pages the scan reads (its SIRead locks) when `pages`.
#[allow(clippy::type_complexity)]
pub fn gist_scan_pages(
    t: &Arc<Table>,
    index: u32,
    orderbys: usize,
    test: &mut dyn FnMut(Option<&crate::catalog::gistsim::Key>, bool) -> Option<Vec<Option<f64>>>,
    pages: bool,
    ctx: &ExecCtx,
) -> Result<Option<(Vec<Vec<u8>>, Vec<u32>)>> {
    let indexes = crate::sql::binder::bound_indexes(t, ctx)?;
    let Some(bix) = indexes.iter().find(|b| b.index.oid == index) else {
        return Ok(None);
    };
    let Some((g, keys)) = gist_model(t, bix, &indexes, ctx)? else {
        return Ok(None);
    };
    let read = if pages { g.search_pages(test) } else { Vec::new() };
    Ok(Some((
        g.search(orderbys, test)
            .into_iter()
            .filter_map(|r| keys.get(r as usize).cloned())
            .collect(),
        read,
    )))
}

/// The leaf page of GiST index `bix` a row with these index values goes to (None: not
/// modelled).
pub fn gist_insert_page(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    vals: &[Value],
    ctx: &ExecCtx,
) -> Result<Option<u32>> {
    let Some(key) = gist_entry_key(bix, vals.first())? else {
        return Ok(None);
    };
    let Some((g, _)) = gist_model(t, bix, indexes, ctx)? else {
        return Ok(None);
    };
    Ok(Some(g.leaf_for(key)))
}

/// A GiST point_ops / range_ops entry's key for an indexed value (outer None: an index the
/// model doesn't cover).
fn gist_entry_key(
    bix: &BoundIndex,
    v: Option<&Value>,
) -> Result<Option<Option<crate::catalog::gistsim::Key>>> {
    use crate::catalog::gistsim::{GBox, Key, RKey};
    use crate::types::geo::Geo;
    if bix.keys.len() != 1 {
        return Ok(None);
    }
    let range = match bix.keys[0].ty() {
        DataType::Point => None,
        DataType::Range(rt) => match rt.oid {
            3904 | 3912 => Some((4u8, 1.0)),
            3926 => Some((8, 1.0)),
            3908 | 3910 => Some((8, 1e6)),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let int = |v: &Option<Value>| -> Option<Option<i64>> {
        match v {
            None => Some(None),
            Some(Value::Int4(i)) | Some(Value::Date(i)) => Some(Some(*i as i64)),
            Some(Value::Int8(i)) | Some(Value::Timestamp(i)) | Some(Value::Timestamptz(i)) => Some(Some(*i)),
            Some(_) => None,
        }
    };
    Ok(Some(match v {
        Some(Value::Geo(g)) if range.is_none() => match &**g {
            Geo::Point(p) => Some(Key::Box(GBox::point(p.x, p.y))),
            _ => return Ok(None),
        },
        Some(Value::Range(r)) if let Some((width, scale)) = range => {
            let (Some(lower), Some(upper)) = (int(&r.lower), int(&r.upper)) else {
                return Ok(None);
            };
            Some(Key::Range(RKey {
                empty: r.empty,
                lower,
                lower_inc: r.lower_inc,
                upper,
                upper_inc: r.upper_inc,
                contain_empty: false,
                width,
                scale,
            }))
        }
        Some(Value::Null) | None => None,
        Some(_) => return Ok(None),
    }))
}

/// The GiST index as its pages hold it now, with the data key of each row its leaf
/// tuples point to (by row number).
fn gist_model(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    ctx: &ExecCtx,
) -> Result<Option<(crate::catalog::gistsim::Gist, Vec<Vec<u8>>)>> {
    use crate::catalog::gistsim::{GBox, Key, RKey, sorted_build_rows};
    use crate::types::geo::Geo;
    if bix.keys.len() != 1 {
        return Ok(None);
    }
    // The range subtype's size and subtype_diff divisor.
    let range = match bix.keys[0].ty() {
        DataType::Point => None,
        DataType::Range(rt) => match rt.oid {
            3904 | 3912 => Some((4u8, 1.0)),
            3926 => Some((8, 1.0)),
            3908 | 3910 => Some((8, 1e6)),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    if !ensure(t, indexes, ctx)? {
        return Ok(None);
    }
    let int = |v: &Option<Value>| -> Option<Option<i64>> {
        match v {
            None => Some(None),
            Some(Value::Int4(i)) | Some(Value::Date(i)) => Some(Some(*i as i64)),
            Some(Value::Int8(i)) | Some(Value::Timestamp(i)) | Some(Value::Timestamptz(i)) => Some(Some(*i)),
            Some(_) => None,
        }
    };
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let raw: Vec<(Vec<u8>, bytes::Bytes)> =
        ctx.txn.borrow().scan(&start, &end).collect::<Result<_>>()?;
    let mut rows: Vec<((u32, u16), Vec<u8>, Option<Key>)> = Vec::new();
    {
        let xids = ctx.sess.db.xids();
        let v = Vis::new(&xids);
        let rf = ctx.sess.db.relfiles.lock();
        let Some(heap) = rf.heaps.get(&t.oid) else {
            return Ok(None);
        };
        for (k, bytes) in &raw {
            let row = decode_table_row(t, bytes)?;
            let Some(vals) = index_values(bix, &row, ctx)? else {
                continue;
            };
            let key = match vals.first() {
                Some(Value::Geo(g)) if range.is_none() => match &**g {
                    Geo::Point(p) => Some(Key::Box(GBox::point(p.x, p.y))),
                    _ => return Ok(None),
                },
                Some(Value::Range(r)) if let Some((width, scale)) = range => {
                    let (Some(lower), Some(upper)) = (int(&r.lower), int(&r.upper)) else {
                        return Ok(None);
                    };
                    Some(Key::Range(RKey {
                        empty: r.empty,
                        lower,
                        lower_inc: r.lower_inc,
                        upper,
                        upper_inc: r.upper_inc,
                        contain_empty: false,
                        width,
                        scale,
                    }))
                }
                Some(Value::Null) | None => None,
                Some(_) => return Ok(None),
            };
            let Some(tid) = heap.locate(k, &v) else {
                continue;
            };
            rows.push((tid, k.clone(), key));
        }
    }
    rows.sort_by_key(|r| r.0);
    // The rows the build read (all of them when it wasn't seen), then the rest.
    let built = ctx.sess.db.relfiles.lock().gin_built.get(&bix.index.oid).and_then(|h| h.first().cloned());
    let (first, later): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|r| built.as_ref().is_none_or(|b| b.contains(&r.1)));
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut g = sorted_build_rows(
        first
            .into_iter()
            .map(|r| {
                keys.push(r.1);
                (r.2, (keys.len() - 1) as u32)
            })
            .collect(),
    );
    for r in later {
        keys.push(r.1);
        g.insert_row(r.2, (keys.len() - 1) as u32);
    }
    Ok(Some((g, keys)))
}

/// VACUUM cleans GIN indexes' pending lists up: the rows there now are in their trees.
pub fn gin_vacuumed(t: &Table, ctx: &ExecCtx) -> Result<()> {
    gin_cleaned(t, None, ctx)
}

/// The pending lists of `t`'s GIN indexes (or just `only`) were cleaned up.
fn gin_cleaned(t: &Table, only: Option<u32>, ctx: &ExecCtx) -> Result<()> {
    let has = {
        let rf = ctx.sess.db.relfiles.lock();
        t.indexes
            .iter()
            .any(|ix| only.is_none_or(|o| o == *ix) && rf.gin_built.contains_key(ix))
    };
    if !has {
        return Ok(());
    }
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let keys: HashSet<Vec<u8>> = ctx
        .txn
        .borrow()
        .scan(&start, &end)
        .map(|r| r.map(|(k, _)| k))
        .collect::<Result<_>>()?;
    let mut rf = ctx.sess.db.relfiles.lock();
    for ix in t.indexes.iter().filter(|ix| only.is_none_or(|o| o == **ix)) {
        if let Some(h) = rf.gin_built.get_mut(ix) {
            let new: HashSet<Vec<u8>> = keys
                .iter()
                .filter(|k| !h.iter().any(|s| s.contains(*k)))
                .cloned()
                .collect();
            if !new.is_empty() {
                h.push(new);
            }
        }
    }
    Ok(())
}

/// The relation's files are gone (DROP).
pub fn dropped(oids: &[u32], db: &Database) {
    let mut rf = db.relfiles.lock();
    for o in oids {
        rf.heaps.remove(o);
        rf.btrees.remove(o);
        rf.untracked.remove(o);
        rf.estimated.remove(o);
        rf.toast.remove(o);
        rf.hash_est.remove(o);
        rf.gin_built.remove(o);
        rf.brin.remove(o);
    }
}

/// The table's relcache entry was rebuilt: its cached insertion target is forgotten.
pub fn invalidated(table: u32, db: &Database) {
    if let Some(h) = db.relfiles.lock().heaps.get_mut(&table) {
        h.invalidate();
    }
}

/// A new heap written in one go (CREATE TABLE AS, a refresh, a table rewrite): rows go in
/// through a bulk insert that skips the free space map (`frozen`: REFRESH MATERIALIZED
/// VIEW's all-visible pages). The indexes are rebuilt afterwards. Returns the rows' heap
/// TIDs (none when the table is too big to model).
pub fn loaded(
    t: &Table,
    rows: &[(Vec<u8>, Vec<Value>)],
    frozen: bool,
    ctx: &ExecCtx,
) -> Vec<Tid> {
    for (k, r) in rows {
        note_compression(t, r, Some(k), ctx);
    }
    if rows.len() > MAX_VERSIONS {
        let est = estimate_rows(t, rows.iter().map(|(_, r)| r.as_slice()), ctx);
        untrack(&ctx.sess.db, t, Some(est));
        return Vec::new();
    }
    let xid = if rows.is_empty() {
        0
    } else {
        ctx.txn.borrow_mut().cur_xid()
    };
    let layouts: Vec<(usize, Vec<NewExt>)> = rows
        .iter()
        .map(|(_, r)| layout(t, r, ctx, &HashSet::new()))
        .collect();
    let xids = ctx.sess.db.xids();
    let mut rf = ctx.sess.db.relfiles.lock();
    rf.untracked.remove(&t.oid);
    rf.estimated.remove(&t.oid);
    forget(&mut rf, t);
    let RelFiles {
        heaps,
        btrees,
        toast,
        ..
    } = &mut *rf;
    let mut h = Heap::new(heap_fillfactor(t));
    let mut mode = InsertMode::bulk();
    mode.frozen = frozen;
    let tt = toast.entry(t.oid).or_default();
    let mut tids = Vec::with_capacity(rows.len());
    for ((k, _), (len, exts)) in rows.iter().zip(layouts) {
        if !exts.is_empty() {
            let saved = save_toast(heaps, btrees, tt, &xids, t.toast, exts, xid, false);
            tt.rows.insert(k.clone(), saved);
        }
        tids.push(h.insert(k.clone(), len, xid, &mut mode));
    }
    heaps.insert(t.oid, h);
    tids
}

/// Rows (by data key) in the order of their heap TIDs where the table's heap is modelled
/// now (those it doesn't place after them, as they were), as a sequential scan reads them.
pub fn in_heap_order<T>(t: &Table, rows: &mut Vec<(Vec<u8>, T)>, ctx: &ExecCtx) {
    let xids = ctx.sess.db.xids();
    let v = Vis::new(&xids);
    let txn = ctx.txn.borrow();
    let mine = |x: u64| txn.is_current_xid(x);
    let locs: Vec<Option<Tid>> = {
        let rf = ctx.sess.db.relfiles.lock();
        let Some(h) = rf.heaps.get(&t.oid) else {
            return;
        };
        rows.iter().map(|(k, _)| h.locate_for(k, &v, &mine)).collect()
    };
    let mut tagged: Vec<(Option<Tid>, usize)> = locs.into_iter().zip(0..).collect();
    tagged.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    let mut slots: Vec<Option<(Vec<u8>, T)>> = rows.drain(..).map(Some).collect();
    rows.extend(tagged.into_iter().filter_map(|(_, i)| slots[i].take()));
}

/// Record the heap TIDs a new heap gave the rows stored under `keys` in the rows
/// themselves (a heap laid out afresh puts them back there).
pub fn stamp_tids(keys: &[Vec<u8>], tids: &[Tid], ctx: &ExecCtx) -> Result<()> {
    for (k, tid) in keys.iter().zip(tids) {
        let cur = ctx.txn.borrow().get(k)?;
        if let Some(bytes) = cur
            && super::scan::stored_tid(&bytes) != Some(*tid)
        {
            let b = super::scan::with_tid(&bytes, *tid)?;
            ctx.txn.borrow_mut().put(k.clone(), b)?;
        }
    }
    Ok(())
}

/// A heap rewritten by VACUUM FULL or CLUSTER: live rows copied in order (physical order,
/// or the clustering index's when `order` gives the keys), pages filled to the fillfactor.
/// The indexes are rebuilt afterwards.
pub fn rewritten(t: &Table, order: Option<&[Vec<u8>]>, ctx: &ExecCtx) -> Result<()> {
    let indexes = crate::sql::binder::bound_indexes(&Arc::new(t.clone()), ctx)?;
    if !ensure(t, &indexes, ctx)? {
        // Too big to model: the live rows packed afresh (measured a row at a time).
        let start = index_prefix(t.oid, 0);
        let mut est = EstHeap::default();
        let ff = heap_fillfactor(t);
        let it = ctx.txn.borrow().scan(&start, &prefix_end(&start));
        for r in it {
            let (_, v) = r?;
            est.add(layout(t, &decode_table_row(t, &v)?, ctx, &HashSet::new()).0, ff);
        }
        ctx.sess.db.relfiles.lock().estimated.insert(t.oid, est);
        return Ok(());
    }
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    let Some(heap) = rf.heaps.get(&t.oid) else {
        return Ok(());
    };
    let v = Vis::new(&xids);
    let mut live = heap.live_rows(&v);
    if let Some(order) = order {
        let pos: HashMap<&[u8], usize> = order
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_slice(), i))
            .collect();
        live.sort_by_key(|(k, _)| pos.get(k.as_slice()).copied().unwrap_or(usize::MAX));
    }
    let old_toast = rf.toast.remove(&t.oid).unwrap_or_default();
    let exts: Vec<(Vec<u8>, Vec<NewExt>)> = live
        .iter()
        .map(|(k, _)| {
            let e = old_toast.rows.get(k).map_or_else(Vec::new, |v| {
                v.iter()
                    .map(|x| NewExt {
                        col: x.col,
                        bytes: x.bytes,
                        sig: x.sig,
                    })
                    .collect()
            });
            (k.clone(), e)
        })
        .collect();
    let keys: Vec<Vec<u8>> = live.iter().map(|(k, _)| k.clone()).collect();
    let (h, tids) = heapsim::rewritten(heap_fillfactor(t), live);
    forget(&mut rf, t);
    rf.heaps.insert(t.oid, h);
    rebuild_toast(&mut rf, t, exts);
    drop(rf);
    // The rows' stored TIDs follow them to their new places.
    for (k, tid) in keys.into_iter().zip(tids) {
        let cur = ctx.txn.borrow().get(&k)?;
        if let Some(bytes) = cur
            && super::scan::stored_tid(&bytes) != Some(tid)
        {
            let b = super::scan::with_tid(&bytes, tid)?;
            ctx.txn.borrow_mut().put(k, b)?;
        }
    }
    Ok(())
}

/// A table's models, taken out while its rows are re-keyed in place.
pub struct Detached {
    heap: Option<Heap>,
    btrees: Vec<(u32, Btree)>,
    toast: Option<Toast>,
}

/// Take a table's models out (its rows are about to be stored under new keys).
pub fn detach(t: &Table, db: &Database) -> Detached {
    let mut rf = db.relfiles.lock();
    let heap = rf.heaps.remove(&t.oid);
    let btrees = t
        .indexes
        .iter()
        .chain(&t.pkey_index)
        .filter_map(|i| rf.btrees.remove(i).map(|b| (*i, b)))
        .collect();
    let toast = rf.toast.remove(&t.oid);
    Detached {
        heap,
        btrees,
        toast,
    }
}

/// Put a table's models back, its rows now stored under new keys (old key, new key).
/// Indexes it no longer has are dropped.
pub fn reattach(t: &Table, d: Detached, rekey: &[(Vec<u8>, Vec<u8>)], db: &Database) {
    let mut rf = db.relfiles.lock();
    let Some(mut heap) = d.heap else {
        // Not modelled: laid out from its rows when next needed.
        rf.heaps.remove(&t.oid);
        return;
    };
    heap.rekey(rekey);
    rf.heaps.insert(t.oid, heap);
    let keep: HashSet<u32> = t.indexes.iter().chain(&t.pkey_index).copied().collect();
    for (i, b) in d.btrees {
        if keep.contains(&i) {
            rf.btrees.insert(i, b);
        }
    }
    if let Some(mut tt) = d.toast {
        let mut rows = HashMap::with_capacity(tt.rows.len());
        for (old, new) in rekey {
            if let Some(e) = tt.rows.remove(old) {
                rows.insert(new.clone(), e);
            }
        }
        tt.rows = rows;
        rf.toast.insert(t.oid, tt);
    }
}

/// A table's rows moved to new keys, unchanged otherwise (`roll_pk_clustered` switched):
/// its GIN indexes' record of the rows their builds read follows them.
pub fn gin_rekeyed(t: &Table, rekey: &[(Vec<u8>, Vec<u8>)], db: &Database) {
    let renamed: HashMap<&[u8], &Vec<u8>> =
        rekey.iter().map(|(o, n)| (o.as_slice(), n)).collect();
    let mut rf = db.relfiles.lock();
    for ix in t.indexes.iter().chain(&t.pkey_index) {
        if let Some(sets) = rf.gin_built.get_mut(ix) {
            for set in sets.iter_mut() {
                *set = set
                    .iter()
                    .map(|k| renamed.get(k.as_slice()).map_or_else(|| k.clone(), |n| (*n).clone()))
                    .collect();
            }
        }
    }
}

/// CREATE INDEX (or a rebuild): the index is built afresh from the table's rows.
/// An index made with its (empty) table: every row is inserted into it later.
pub fn index_created(index: u32, ctx: &ExecCtx) {
    let mut rf = ctx.sess.db.relfiles.lock();
    rf.gin_built.insert(index, vec![HashSet::new()]);
    rf.brin.insert(index, BrinRanges::default());
}

pub fn index_built(t: &Table, index: u32, ctx: &ExecCtx) -> Result<()> {
    // A hash index's buckets come from estimate_rel_size's tuples at build time (kept
    // for any index: the new one may not be in the catalog yet).
    let est = crate::sql::costs::rel_size(t, ctx).tuples;
    // A GIN index's build reads the rows there now (kept for any index, as above).
    {
        let start = index_prefix(t.oid, 0);
        let end = prefix_end(&start);
        let keys: HashSet<Vec<u8>> = ctx
            .txn
            .borrow()
            .scan(&start, &end)
            .map(|r| r.map(|(k, _)| k))
            .collect::<Result<_>>()?;
        ctx.sess.db.relfiles.lock().gin_built.insert(index, vec![keys]);
    }
    {
        let mut rf = ctx.sess.db.relfiles.lock();
        rf.btrees.remove(&index);
        rf.hash_est.insert(index, est);
        if let Some(h) = rf.heaps.get_mut(&t.oid) {
            h.invalidate();
        }
    }
    let indexes = crate::sql::binder::bound_indexes(&Arc::new(t.clone()), ctx)?;
    ensure(t, &indexes, ctx)?;
    // A BRIN build summarizes every range of the heap's pages (the first even when empty).
    let blocks = heap_blocks(t, ctx)?.unwrap_or(0);
    ctx.sess.db.relfiles.lock().brin.insert(
        index,
        BrinRanges {
            built_blocks: blocks,
            ..Default::default()
        },
    );
    Ok(())
}

/// A BRIN index's pages_per_range.
pub fn brin_pages_per_range(ix: &crate::catalog::Index) -> u32 {
    option(&ix.options, "pages_per_range")
        .and_then(|v| v.parse().ok())
        .filter(|n: &u32| *n > 0)
        .unwrap_or(128)
}

/// brinsummarize: summarize the unsummarized ranges of BRIN index `ix` on `t` (just the one
/// holding block `only`, when given); a last range the heap doesn't fill only with
/// `partial`. The ranges summarized.
pub fn brin_summarize(
    t: &Table,
    ix: &crate::catalog::Index,
    only: Option<u32>,
    partial: bool,
    ctx: &ExecCtx,
) -> Result<i64> {
    let ppr = brin_pages_per_range(ix);
    let blocks = heap_blocks(t, ctx)?.unwrap_or(0);
    let mut rf = ctx.sess.db.relfiles.lock();
    let st = rf.brin.entry(ix.oid).or_insert_with(|| BrinRanges {
        built_blocks: blocks,
        ..Default::default()
    });
    let mut n = 0;
    for r in 0..blocks.div_ceil(ppr) {
        if only.is_some_and(|b| b / ppr != r) || st.summarized(r, ppr) {
            continue;
        }
        if !partial && (r as u64 + 1) * ppr as u64 > blocks as u64 {
            continue;
        }
        st.removed.remove(&r);
        st.added.insert(r);
        n += 1;
    }
    Ok(n)
}

/// brin_desummarize_range: the range holding heap block `block` loses its summary.
pub fn brin_desummarize(t: &Table, ix: &crate::catalog::Index, block: u32, ctx: &ExecCtx) -> Result<()> {
    let ppr = brin_pages_per_range(ix);
    let blocks = heap_blocks(t, ctx)?.unwrap_or(0);
    let mut rf = ctx.sess.db.relfiles.lock();
    let st = rf.brin.entry(ix.oid).or_insert_with(|| BrinRanges {
        built_blocks: blocks,
        ..Default::default()
    });
    let r = block / ppr;
    st.added.remove(&r);
    st.removed.insert(r);
    Ok(())
}

/// VACUUM summarizes BRIN indexes' unsummarized ranges the heap fills
/// (brinvacuumcleanup).
pub fn brin_vacuumed(t: &Table, ctx: &ExecCtx) -> Result<()> {
    let cat = ctx.catalog();
    for oid in &t.indexes {
        if let Some(ix) = cat.indexes.get(oid).filter(|ix| ix.method == "brin") {
            brin_summarize(t, ix, None, false, ctx)?;
        }
    }
    Ok(())
}

thread_local! {
    /// The running VACUUM's TRUNCATE option (None: not given).
    static VACUUM_TRUNCATE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// VACUUM's TRUNCATE option, for the tables it vacuums next.
pub fn set_vacuum_truncate(v: Option<bool>) {
    VACUUM_TRUNCATE.with(|c| c.set(v));
}

/// What VACUUM found and left.
pub struct Vacuumed {
    pub pages: i32,
    pub tuples: f64,
    pub allvisible: i32,
    /// Indexes it cleaned up: (oid, pages, tuples).
    pub indexes: Vec<(u32, i32, f64)>,
}

/// Lazy VACUUM of a table (and of its TOAST table).
pub fn vacuum(t: &Table, ctx: &ExecCtx) -> Result<Option<Vacuumed>> {
    gin_vacuumed(t, ctx)?;
    brin_vacuumed(t, ctx)?;
    let indexes = crate::sql::binder::bound_indexes(&Arc::new(t.clone()), ctx)?;
    if !ensure(t, &indexes, ctx)? {
        return Ok(None);
    }
    let db = &ctx.sess.db;
    let xids = db.xids();
    let mut rf = db.relfiles.lock();
    let RelFiles { heaps, btrees, .. } = &mut *rf;
    let Some(heap) = heaps.get_mut(&t.oid) else {
        return Ok(None);
    };
    let v = vis_for(&xids, t.oid, ctx);
    // VACUUM's TRUNCATE option, else the vacuum_truncate storage parameter (its toast.
    // one for the TOAST table).
    // (Else the vacuum_truncate setting.)
    let opt = VACUUM_TRUNCATE.with(|c| c.get());
    let guc = ctx.sess.settings.borrow().bool("vacuum_truncate");
    let truncate = |name: &str| {
        opt.unwrap_or_else(|| match option(&t.options, name) {
            Some(v) => crate::types::text::parse_bool(&v).unwrap_or(true),
            None => guc,
        })
    };
    let res = heap.vacuum(&v, indexes.len(), truncate("vacuum_truncate"));
    let mut out = Vacuumed {
        pages: res.pages as i32,
        tuples: res.live,
        allvisible: res.all_visible as i32,
        indexes: Vec::new(),
    };
    if !res.dead.is_empty() {
        let dead: HashSet<Tid> = res.dead.iter().copied().collect();
        for bix in &indexes {
            if let Some(bt) = btrees.get_mut(&bix.index.oid) {
                bt.bulk_delete(&dead);
                let tuples = (bt.tuples as f64).min(res.live);
                out.indexes
                    .push((bix.index.oid, bt.nblocks() as i32, tuples));
            }
        }
    }
    if t.toast != 0
        && let Some(th) = heaps.get_mut(&t.toast)
    {
        let r = th.vacuum(&v, 1, truncate("toast.vacuum_truncate"));
        if !r.dead.is_empty()
            && let Some(bt) = btrees.get_mut(&(t.toast + 1))
        {
            bt.bulk_delete(&r.dead.iter().copied().collect());
        }
    }
    Ok(Some(out))
}

/// The table's size in pages and its all-visible pages, as ANALYZE (or CREATE INDEX)
/// counts them, with its btree indexes' sizes.
pub fn measured(t: &Table, ctx: &ExecCtx) -> Result<Option<(i32, i32, HashMap<u32, i32>)>> {
    let indexes = crate::sql::binder::bound_indexes(&Arc::new(t.clone()), ctx)?;
    if !ensure(t, &indexes, ctx)? {
        return Ok(None);
    }
    let rf = ctx.sess.db.relfiles.lock();
    let Some(heap) = rf.heaps.get(&t.oid) else {
        return Ok(None);
    };
    let ix = indexes
        .iter()
        .filter_map(|b| {
            rf.btrees
                .get(&b.index.oid)
                .map(|bt| (b.index.oid, bt.nblocks() as i32))
        })
        .collect();
    Ok(Some((
        heap.nblocks() as i32,
        heap.all_visible_pages() as i32,
        ix,
    )))
}

/// A btree index's root level, if modelled.
pub fn btree_height(index: u32, db: &Database) -> Option<u32> {
    db.relfiles.lock().btrees.get(&index).map(Btree::height)
}

/// The leaf pages of btree `index` an index scan over these entry ranges (with the
/// index's key prefix) reads; None when the index isn't modelled.
pub fn btree_leaf_pages(
    index: u32,
    prefix: &[u8],
    ranges: &[(Vec<u8>, Vec<u8>)],
    db: &Database,
) -> Option<Vec<u32>> {
    let rf = db.relfiles.lock();
    let bt = rf.btrees.get(&index)?;
    let mut out: Vec<u32> = Vec::new();
    for (lo, hi) in ranges {
        for p in bt.leaf_pages(lo.strip_prefix(prefix), hi.strip_prefix(prefix)) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    Some(out)
}

/// The index pages a row's new entries go to: (index, leaf page; None when the index
/// isn't modelled as pages, a non-btree one).
pub fn entry_pages(
    t: &Table,
    indexes: &[BoundIndex],
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<Vec<(u32, Option<u32>)>> {
    let mut out: Vec<(u32, Option<u32>)> = Vec::new();
    let modelled = ensure(t, indexes, ctx)?;
    let es = entries(t, indexes, row, ctx)?;
    let table = Arc::new(t.clone());
    for bix in indexes {
        if bix.index.id == 0 {
            continue;
        }
        let page = match bix.index.method.as_str() {
            "gin" if modelled => {
                // (A row's keys: one page each.)
                let pages = match index_values(bix, row, ctx)? {
                    Some(v) => gin_ssi_pages(&table, bix, indexes, &v, true, ctx)?,
                    None => None,
                };
                match pages {
                    Some(ps) if !ps.is_empty() => {
                        out.extend(ps.into_iter().map(|p| (bix.index.oid, Some(p))));
                        continue;
                    }
                    _ => None,
                }
            }
            "gist" if modelled => match index_values(bix, row, ctx)? {
                Some(v) => gist_insert_page(&table, bix, indexes, &v, ctx)?,
                None => None,
            },
            "hash" if modelled => match index_values(bix, row, ctx)? {
                Some(v) if v.len() == 1 && !v[0].is_null() => {
                    hash_bucket(&table, bix, indexes, &v[0], ctx)?
                }
                _ => None,
            },
            _ => {
                let rf = ctx.sess.db.relfiles.lock();
                es.iter()
                    .find(|(ix, _)| *ix == bix.index.oid)
                    .filter(|_| modelled)
                    .and_then(|(ix, k)| rf.btrees.get(ix).and_then(|bt| bt.leaf_for(k).or(Some(0))))
            }
        };
        out.push((bix.index.oid, page));
    }
    Ok(out)
}

/// A btree index's size in pages, if modelled.
pub fn btree_blocks(index: u32, db: &Database) -> Option<u32> {
    db.relfiles.lock().btrees.get(&index).map(Btree::nblocks)
}

/// Where each row (by data key) is in the table's heap, and whether the visibility map
/// marks its page all-visible (None: the heap isn't modelled).
pub fn row_locations(
    t: &Table,
    keys: &[Vec<u8>],
    ctx: &ExecCtx,
) -> Option<Vec<Option<(Tid, bool)>>> {
    heap_blocks(t, ctx).ok().flatten()?;
    let xids = ctx.sess.db.xids();
    let v = Vis::new(&xids);
    let txn = ctx.txn.borrow();
    let mine = |x: u64| txn.is_current_xid(x);
    let rf = ctx.sess.db.relfiles.lock();
    let h = rf.heaps.get(&t.oid)?;
    // (A version whose insert aborted leads back to the one it replaced.)
    Some(
        keys.iter()
            .map(|k| h.locate_for(k, &v, &mine).map(|tid| (tid, h.page_all_visible(tid.0))))
            .collect(),
    )
}

/// The heap TIDs of stored rows (data key, bytes) as a scan reads them: the TID each
/// version was written at (what its ctid shows), else (a row stored before its heap was
/// laid out afresh) where the model has it. None when the table's heap isn't modelled.
pub fn row_tids(t: &Table, rows: &[(Vec<u8>, bytes::Bytes)], ctx: &ExecCtx) -> Option<Vec<Option<Tid>>> {
    heap_blocks(t, ctx).ok().flatten()?;
    let mut out: Vec<Option<Tid>> = rows
        .iter()
        .map(|(_, b)| super::scan::stored_tid(b))
        .collect();
    let rf = ctx.sess.db.relfiles.lock();
    let h = rf.heaps.get(&t.oid)?;
    if h.unstamped || out.iter().any(Option::is_none) {
        let xids = ctx.sess.db.xids();
        let v = Vis::new(&xids);
        for (o, (k, _)) in out.iter_mut().zip(rows) {
            if o.is_none() || h.unstamped {
                *o = h.locate(k, &v);
            }
        }
    }
    Some(out)
}

/// The rows' locations as a bitmap heap scan reads them: their HOT chains' roots (what
/// the indexes point to).
pub fn row_roots(t: &Table, keys: &[Vec<u8>], ctx: &ExecCtx) -> Option<Vec<Option<Tid>>> {
    heap_blocks(t, ctx).ok().flatten()?;
    let xids = ctx.sess.db.xids();
    let v = Vis::new(&xids);
    let txn = ctx.txn.borrow();
    let mine = |x: u64| txn.is_current_xid(x);
    let rf = ctx.sess.db.relfiles.lock();
    let h = rf.heaps.get(&t.oid)?;
    Some(keys.iter().map(|k| h.locate_for(k, &v, &mine).map(|tid| h.hot_root(tid))).collect())
}

/// Whether the table's heap is modelled page by page (`row_tids` places its rows),
/// deciding it on first use.
pub fn heap_modelled(t: &Table, ctx: &ExecCtx) -> bool {
    matches!(heap_blocks(t, ctx), Ok(Some(_))) && ctx.sess.db.relfiles.lock().heaps.contains_key(&t.oid)
}

/// The table's current size in pages (None: not modelled).
pub fn heap_blocks(t: &Table, ctx: &ExecCtx) -> Result<Option<u32>> {
    let indexes = crate::sql::binder::bound_indexes(&Arc::new(t.clone()), ctx)?;
    if !ensure(t, &indexes, ctx)? {
        return Ok(ctx.sess.db.relfiles.lock().estimated.get(&t.oid).map(|e| e.pages));
    }
    Ok(ctx
        .sess
        .db
        .relfiles
        .lock()
        .heaps
        .get(&t.oid)
        .map(Heap::nblocks))
}

// ── COPY ──

/// A COPY FROM into `t` in progress: rows go in through a bulk insert state, buffered
/// into multi-inserts when `multi`. Dropping it before `finish` discards the buffer (the
/// command failed).
pub struct CopyLoad {
    db: Arc<Database>,
    key: (u32, u64),
    toast: u32,
}

impl CopyLoad {
    pub fn start(t: &Table, multi: bool, ctx: &ExecCtx) -> CopyLoad {
        let txn = ctx.txn.borrow();
        let key = (t.oid, txn.id);
        // A table new in this transaction isn't worth looking up free space in.
        let use_fsm = !txn.storage_new_in_txn(t.oid);
        let db = ctx.sess.db.clone();
        db.relfiles.lock().bulk.insert(
            key,
            Bulk {
                mode: InsertMode {
                    use_fsm,
                    ..InsertMode::bulk()
                },
                batch: multi.then(Batch::default),
            },
        );
        CopyLoad {
            db,
            key,
            toast: t.toast,
        }
    }

    /// A line was read and its row stored (or skipped): the buffer is flushed once full.
    pub fn row_done(&self, line_len: usize, ctx: &ExecCtx) {
        let full = {
            let mut rf = self.db.relfiles.lock();
            match rf.bulk.get_mut(&self.key).and_then(|b| b.batch.as_mut()) {
                Some(b) if !b.rows.is_empty() => {
                    b.bytes += line_len;
                    b.rows.len() >= COPY_TUPLES || b.bytes >= COPY_BYTES
                }
                _ => false,
            }
        };
        if full {
            self.flush(ctx);
        }
    }

    fn flush(&self, ctx: &ExecCtx) {
        let xid = ctx.txn.borrow_mut().cur_xid();
        let cid = ctx.txn.borrow_mut().output_cid();
        let xids = self.db.xids();
        let mut rf = self.db.relfiles.lock();
        let RelFiles {
            heaps,
            btrees,
            bulk,
            toast,
            ..
        } = &mut *rf;
        let Some(b) = bulk.get_mut(&self.key) else {
            return;
        };
        let Some(batch) = b.batch.as_mut() else {
            return;
        };
        let mut rows = std::mem::take(&mut batch.rows);
        batch.bytes = 0;
        if rows.is_empty() || !heaps.contains_key(&self.key.0) {
            return;
        }
        // Each tuple is toasted first, then they all go in.
        for p in &mut rows {
            if !p.exts.is_empty() {
                let tt = toast.entry(self.key.0).or_default();
                let exts = std::mem::take(&mut p.exts);
                let saved =
                    save_toast(heaps, btrees, tt, &xids, self.toast, exts, xid, b.mode.use_fsm);
                tt.rows.insert(p.key.clone(), saved);
            }
        }
        let Some(heap) = heaps.get_mut(&self.key.0) else {
            return;
        };
        let tids = heap.multi_insert(
            rows.iter().map(|p| (p.key.clone(), p.len)).collect(),
            xid,
            &mut b.mode,
        );
        for tid in &tids {
            heap.set_raw_cid(*tid, cid);
        }
        let mut keys = Vec::with_capacity(rows.len());
        for (p, tid) in rows.into_iter().zip(&tids) {
            let entries = p.entries.into_iter().map(|(o, k)| (o, k, false)).collect();
            index_insert(btrees, heap, &xids, entries, *tid);
            keys.push(p.key);
        }
        drop(rf);
        // The rows (stored already, as each line was read) get the TIDs their tuples went to,
        // as single inserts' rows do (their ctid; scans read them in that order).
        let _ = stamp_tids(&keys, &tids, ctx);
    }

    /// The COPY is done: the buffer goes in.
    pub fn finish(&self, ctx: &ExecCtx) {
        self.flush(ctx);
    }
}

impl Drop for CopyLoad {
    fn drop(&mut self) {
        self.db.relfiles.lock().bulk.remove(&self.key);
    }
}

// ── sizes ──

/// A relation fork.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fork {
    Main,
    Fsm,
    Vm,
    Init,
}

impl Fork {
    pub const ALL: [Fork; 4] = [Fork::Main, Fork::Fsm, Fork::Vm, Fork::Init];

    pub fn parse(s: &str) -> Result<Fork> {
        Ok(match s {
            "main" => Fork::Main,
            "fsm" => Fork::Fsm,
            "vm" => Fork::Vm,
            "init" => Fork::Init,
            _ => {
                return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid fork name")
                    .hint("Valid fork names are \"main\", \"fsm\", \"vm\", and \"init\"."));
            }
        })
    }
}

/// What a relation oid names, as far as its files go.
enum Rel {
    /// A table (or a materialized view's storage) with storage.
    Heap(Arc<Table>),
    /// An index of a table.
    Index(Arc<Table>, u32),
    /// A TOAST table of a table, or (`true`) its index.
    Toast(Arc<Table>, bool),
    /// A relation with a single page (a sequence), unlogged or not.
    OnePage(bool),
    /// No storage (a view, a partitioned table or index, a composite type).
    None,
}

fn rel_of(cat: &Catalog, oid: u32) -> Option<Rel> {
    if let Some(t) = cat.tables.get(&oid) {
        return Some(if t.partkey.is_some() {
            Rel::None
        } else {
            Rel::Heap(t.clone())
        });
    }
    if let Some(v) = cat.views.get(&oid) {
        return Some(match &v.storage {
            Some(st) if v.materialized => Rel::Heap(st.clone()),
            _ => Rel::None,
        });
    }
    if let Some(ix) = cat.indexes.get(&oid) {
        let owner = cat.tables.get(&ix.table).cloned().or_else(|| {
            cat.views
                .get(&ix.table)
                .and_then(|v| v.storage.clone())
        });
        return Some(match owner {
            Some(t) if t.partkey.is_none() => Rel::Index(t, oid),
            _ => Rel::None,
        });
    }
    if let Some((t, is_index)) = cat.toast_owner(oid) {
        return Some(Rel::Toast(t.clone(), is_index));
    }
    if cat.sequences.contains_key(&oid) {
        return Some(Rel::OnePage(false));
    }
    None
}

/// A heap's blocks in each fork, when not modelled: an estimate from its rows.
fn estimated_heap(t: &Table, ctx: &ExecCtx) -> Result<u32> {
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let raw: Vec<(Vec<u8>, bytes::Bytes)> =
        ctx.txn.borrow().scan(&start, &end).collect::<Result<_>>()?;
    let mut lens = Vec::with_capacity(raw.len());
    for (_, v) in raw {
        let r = decode_table_row(t, &v)?;
        let len = layout(t, &r, ctx, &HashSet::new()).0;
        lens.push(heapsim::maxalign(len) + heapsim::ITEM_ID);
    }
    Ok(relsize::heap_pages(lens.into_iter()).max(0) as u32)
}

/// Blocks in one fork of a relation (None: no such relation).
pub fn fork_blocks(oid: u32, fork: Fork, ctx: &ExecCtx) -> Result<Option<u64>> {
    let cat = ctx.catalog();
    let Some(rel) = rel_of(&cat, oid) else {
        // A system relation: its size in a fresh database.
        if let Some((main, fsm, vm)) = crate::catalog::sys_sizes::sys_size(oid) {
            return Ok(Some(match fork {
                Fork::Main => main,
                Fork::Fsm => fsm,
                Fork::Vm => vm,
                Fork::Init => 0,
            } as u64));
        }
        // Otherwise a table or index has a page, a view none.
        let Some(r) = crate::catalog::sys_rels::SYS_RELS.iter().find(|r| r.2 == oid) else {
            return Ok(None);
        };
        return Ok(Some((fork == Fork::Main && matches!(r.5, 'r' | 'i' | 't')) as u64));
    };
    Ok(Some(match rel {
        Rel::None => 0,
        Rel::OnePage(unlogged) => match fork {
            Fork::Main => 1,
            Fork::Init if unlogged => 1,
            _ => 0,
        },
        Rel::Heap(t) => {
            if fork == Fork::Init {
                return Ok(Some(0));
            }
            let indexes = crate::sql::binder::bound_indexes(&t, ctx)?;
            if !ensure(&t, &indexes, ctx)? {
                let est = ctx.sess.db.relfiles.lock().estimated.get(&t.oid).map(|e| e.pages);
                return Ok(Some(match fork {
                    Fork::Main => match est {
                        Some(p) => p as u64,
                        None => estimated_heap(&t, ctx)? as u64,
                    },
                    _ => 0,
                }));
            }
            let rf = ctx.sess.db.relfiles.lock();
            let h = rf.heaps.get(&t.oid);
            h.map_or(0, |h| match fork {
                Fork::Main => h.nblocks(),
                Fork::Fsm => h.fsm_blocks(),
                Fork::Vm => h.vm_blocks(),
                Fork::Init => 0,
            }) as u64
        }
        Rel::Index(t, ix) => {
            let indexes = crate::sql::binder::bound_indexes(&t, ctx)?;
            let Some(bix) = indexes.iter().find(|b| b.index.oid == ix) else {
                return Ok(Some(0));
            };
            let unlogged = t.unlogged;
            if bix.index.method == "hash"
                && fork == Fork::Main
                && let Some(n) = hash_index_pages(&t, bix, &indexes, ctx)?
            {
                return Ok(Some(n));
            }
            if bix.index.method == "gin"
                && fork == Fork::Main
                && let Some(n) = gin_index_pages(&t, bix, &indexes, ctx)?
            {
                return Ok(Some(n));
            }
            if bix.index.method == "gist"
                && fork == Fork::Main
                && let Some(n) = gist_index_pages(&t, bix, &indexes, ctx)?
            {
                return Ok(Some(n));
            }
            if btree_opts(bix).is_none() {
                // Other access methods aren't modelled: the size they have empty.
                let empty = match bix.index.method.as_str() {
                    "hash" => 10,
                    "gin" => 2,
                    "brin" | "spgist" => 3,
                    _ => 1,
                };
                return Ok(Some(match fork {
                    Fork::Main => empty,
                    Fork::Init if unlogged => 1,
                    _ => 0,
                }));
            }
            match fork {
                Fork::Init => return Ok(Some(unlogged as u64)),
                Fork::Fsm | Fork::Vm => return Ok(Some(0)),
                Fork::Main => {}
            }
            if !ensure(&t, &indexes, ctx)? {
                return Ok(Some(1));
            }
            let rf = ctx.sess.db.relfiles.lock();
            rf.btrees.get(&ix).map_or(1, |b| b.nblocks()) as u64
        }
        Rel::Toast(t, is_index) => {
            let rf = ctx.sess.db.relfiles.lock();
            if is_index {
                match fork {
                    Fork::Main => rf.btrees.get(&(t.toast + 1)).map_or(1, |b| b.nblocks()) as u64,
                    Fork::Init => t.unlogged as u64,
                    _ => 0,
                }
            } else {
                rf.heaps.get(&t.toast).map_or(0, |h| match fork {
                    Fork::Main => h.nblocks(),
                    Fork::Fsm => h.fsm_blocks(),
                    Fork::Vm => h.vm_blocks(),
                    Fork::Init => 0,
                }) as u64
            }
        }
    }))
}

/// The bucket of hash index `bix` value `v` goes to (a page id for SSI: the bucket's
/// primary page), the index as its rows left it; None when not modelled.
pub fn hash_bucket(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    v: &Value,
    ctx: &ExecCtx,
) -> Result<Option<u32>> {
    let Some((est, ff, codes)) = hash_codes(t, bix, indexes, ctx)? else {
        return Ok(None);
    };
    let code = hash_code(v, &bix.keys[0].ty());
    Ok(Some(crate::catalog::hashidx::bucket_of_code(est, ff, codes.into_iter(), code) + 1))
}

fn hash_code(v: &Value, ty: &crate::types::DataType) -> u32 {
    crate::catalog::hashidx::hash_value(v, ty).unwrap_or_else(|| {
        let mut b = Vec::new();
        encode_value(&mut b, v, false);
        crate::catalog::hashidx::hash_bytes(&b)
    })
}

/// A hash index's pages: its live rows' hash codes inserted in heap order into the
/// buckets it was built with.
fn hash_index_pages(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    ctx: &ExecCtx,
) -> Result<Option<u64>> {
    let Some((est, ff, codes)) = hash_codes(t, bix, indexes, ctx)? else {
        return Ok(None);
    };
    Ok(Some(crate::catalog::hashidx::index_pages(est, ff, codes.into_iter())))
}

/// A hash index's build estimate, fillfactor and its live rows' codes in heap order.
#[allow(clippy::type_complexity)]
fn hash_codes(
    t: &Arc<Table>,
    bix: &BoundIndex,
    indexes: &[BoundIndex],
    ctx: &ExecCtx,
) -> Result<Option<(f64, usize, Vec<u32>)>> {
    if bix.keys.len() != 1 || !ensure(t, indexes, ctx)? {
        return Ok(None);
    }
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let raw: Vec<(Vec<u8>, bytes::Bytes)> =
        ctx.txn.borrow().scan(&start, &end).collect::<Result<_>>()?;
    let ty = bix.keys[0].ty();
    let mut codes: Vec<(Option<Tid>, u32)> = Vec::new();
    let mut keys = Vec::new();
    for (k, v) in &raw {
        let row = decode_table_row(t, v)?;
        let Some(vals) = index_values(bix, &row, ctx)? else {
            continue;
        };
        let Some(v) = vals.first().filter(|v| !v.is_null()) else {
            continue;
        };
        codes.push((None, hash_code(v, &ty)));
        keys.push(k.clone());
    }
    let est = {
        let rf = ctx.sess.db.relfiles.lock();
        if let Some(h) = rf.heaps.get(&t.oid) {
            for (c, k) in codes.iter_mut().zip(&keys) {
                c.0 = h.tid_of(k);
            }
        }
        rf.hash_est.get(&bix.index.oid).copied().unwrap_or(f64::NAN)
    };
    // (Inserted in heap order.)
    codes.sort_by_key(|c| c.0.map_or((u32::MAX, u16::MAX), |t| (t.0, t.1)));
    let est = if est.is_nan() {
        crate::sql::costs::rel_size(t, ctx).tuples
    } else {
        est
    };
    let ff = option(&bix.index.options, "fillfactor")
        .and_then(|v| v.parse().ok())
        .unwrap_or(75);
    Ok(Some((est, ff, codes.into_iter().map(|c| c.1).collect())))
}

/// All forks of a relation, in bytes (None: no such relation).
fn rel_bytes(oid: u32, ctx: &ExecCtx) -> Result<Option<i64>> {
    let mut total = 0;
    for f in Fork::ALL {
        match fork_blocks(oid, f, ctx)? {
            Some(b) => total += b,
            None => return Ok(None),
        }
    }
    Ok(Some((total * heapsim::BLCKSZ as u64) as i64))
}

/// pg_relation_size.
pub fn relation_size(oid: u32, fork: Fork, ctx: &ExecCtx) -> Result<Option<i64>> {
    Ok(fork_blocks(oid, fork, ctx)?.map(|b| (b * heapsim::BLCKSZ as u64) as i64))
}

/// pg_table_size: every fork, with the TOAST table and its index.
pub fn table_size(oid: u32, ctx: &ExecCtx) -> Result<Option<i64>> {
    let Some(mut n) = rel_bytes(oid, ctx)? else {
        return Ok(None);
    };
    let cat = ctx.catalog();
    if let Some(Rel::Heap(t)) = rel_of(&cat, oid)
        && t.toast != 0
    {
        n += rel_bytes(t.toast, ctx)?.unwrap_or(0);
        n += rel_bytes(t.toast + 1, ctx)?.unwrap_or(0);
    }
    Ok(Some(n))
}

/// pg_indexes_size.
pub fn indexes_size(oid: u32, ctx: &ExecCtx) -> Result<Option<i64>> {
    let cat = ctx.catalog();
    let Some(rel) = rel_of(&cat, oid) else {
        return Ok(rel_bytes(oid, ctx)?.map(|_| 0));
    };
    let t = match rel {
        Rel::Heap(t) => Some(t),
        _ => cat.tables.get(&oid).cloned(),
    };
    let mut n = 0;
    if let Some(t) = t {
        for i in t.indexes.iter().chain(&t.pkey_index) {
            n += rel_bytes(*i, ctx)?.unwrap_or(0);
        }
    }
    Ok(Some(n))
}

/// pg_total_relation_size.
pub fn total_size(oid: u32, ctx: &ExecCtx) -> Result<Option<i64>> {
    let Some(t) = table_size(oid, ctx)? else {
        return Ok(None);
    };
    Ok(Some(t + indexes_size(oid, ctx)?.unwrap_or(0)))
}
