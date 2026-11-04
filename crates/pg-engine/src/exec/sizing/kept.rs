//! Heap and btree models kept across a clean close of their database, so a table's first
//! statement after a restart doesn't read and decode every row to lay its models out again.
//!
//! What is kept is what `ensure` would compute from the stored rows: each live row's key,
//! tuple length, TID and out-of-line values, and each btree index's key per row. Loading it
//! runs the same layout (`heapsim::restored`, `Btree::build`) as reading the rows does.
//!
//! - Written when the database closes cleanly (`persist`), from the models it has then,
//!   one record per table under `[0; 7] ++ [0x23] ++ oid`, then a marker (`[0; 7] ++ [0x24]`).
//!   A table whose kept record is still the one loaded isn't written again.
//! - Valid only while the marker is there. Opening the database (`open`) deletes it before
//!   any session can write, so after a crash nothing kept is trusted: the records are
//!   deleted then and models are laid out from the rows as before. A record must also match
//!   the table's relfilenode and definition (its columns, options and indexes).
//! - A table whose model is dropped, truncated or rewritten loses its record (`forget`).

use super::*;
use crate::storage::keys::prefix_end;
use std::hash::Hasher;

const PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x23];
const MARKER: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0x24];
/// 2: the heap's shape and each btree's pages (1: each btree's keys per row).
const VERSION: u8 = 2;
/// A record's first byte: the rest as it is, or compressed (zstd).
const RAW: u8 = 0xf0;
const ZSTD: u8 = 0xf1;

fn key(oid: u32) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(&oid.to_be_bytes());
    k
}

/// What `ensure` lays a table's models out from: (key, tuple length, TID) per row in TID
/// order, its out-of-line values, the heap's shape (`Heap::read_shape`), and its btree
/// indexes as their pages were.
pub(super) struct Restore {
    pub rows: KeptRows,
    pub exts: Vec<(Vec<u8>, Vec<NewExt>)>,
    pub shape: Vec<u8>,
    pub btrees: HashMap<u32, Btree>,
}

/// A record's rows (key, tuple length, TID), their keys in one buffer: decoding makes no
/// allocation per row.
#[derive(Default)]
pub(super) struct KeptRows {
    keys: Vec<u8>,
    /// Per row: where its key ends in `keys`, its tuple length, its TID.
    rows: Vec<(u32, u32, Tid)>,
}

impl KeptRows {
    fn key(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.rows[i - 1].0 as usize };
        &self.keys[start..self.rows[i].0 as usize]
    }

    /// The heap model the rows give (`heapsim::restored`).
    pub fn restored(&self, fillfactor: usize) -> (Heap, bool, Vec<Tid>) {
        let rows = (0..self.rows.len()).map(|i| (self.key(i), self.rows[i].1 as usize, Some(self.rows[i].2)));
        heapsim::restored_from(fillfactor, self.rows.len(), self.keys.len(), rows)
    }
}

/// The table's definition as far as its models depend on it (and a little more: a rename
/// only costs a layout from the rows).
fn token(cat: &Catalog, t: &Table) -> u64 {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.write(&serde_json::to_vec(t).unwrap_or_default());
    for ix in index_oids(t).iter().filter_map(|o| cat.indexes.get(o)) {
        h.write(&serde_json::to_vec(&**ix).unwrap_or_default());
    }
    h.finish()
}

/// The table's indexes (its primary key's too), by oid.
fn index_oids(t: &Table) -> Vec<u32> {
    let mut v: Vec<u32> = t.indexes.iter().chain(&t.pkey_index).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// FNV-1a: a hash that stays the same across builds.
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
}

/// The database opened: whether what its last clean close kept may be used. Deletes the
/// marker (and, when there was none, every kept record) before any session can write.
/// `reset`: unlogged relations just emptied.
pub fn open(db: &Database, reset: &[u32]) -> Result<()> {
    let store = &db.store;
    let ts = store.visible_ts();
    let mut gone: Vec<(Vec<u8>, Option<bytes::Bytes>)> = Vec::new();
    let mut stored = HashSet::new();
    match store.get(MARKER, ts)? {
        // (The marker lists the tables kept.)
        Some(m) => {
            stored.extend(m.as_chunks::<4>().0.iter().map(|o| u32::from_be_bytes(*o)));
            gone.push((MARKER.to_vec(), None));
        }
        None => {
            for r in store.scan(PREFIX, &prefix_end(PREFIX), ts) {
                gone.push((r?.0, None));
            }
        }
    }
    let kept: HashSet<u32> = stored.iter().copied().filter(|o| !reset.contains(o)).collect();
    if !gone.is_empty() {
        store.commit(gone, |_| Ok(()))?;
    }
    for shard in db.relfiles.all() {
        let mut rf = shard.write();
        rf.kept.clear();
        rf.kept_stored.clear();
        rf.kept_sizes.clear();
    }
    for oid in kept {
        db.relfiles.of(oid).write().kept.insert(oid);
    }
    for oid in stored {
        db.relfiles.of(oid).write().kept_stored.insert(oid);
    }
    Ok(())
}

/// The table's model is laid out again or gone: what was kept for it no longer is.
pub(super) fn forget(rf: &mut RelFiles, oid: u32) {
    rf.kept.remove(&oid);
    rf.building.remove(&oid);
    rf.kept_hash.remove(&oid);
    rf.kept_sizes.remove(&oid);
}

/// What `take` found for a table.
pub(super) enum Take<'a> {
    /// Nothing kept (or no longer valid): its models are laid out from the rows.
    None,
    /// The background builder (`build_in_background`) is laying them out: wait for it.
    Wait,
    /// Its record, while the table is marked as being laid out (`Building`).
    Restore(Restore, Building<'a>),
}

/// A table whose kept models are being laid out: until it is dropped, others needing them
/// wait (`Models::wait_built`) instead of laying them out again.
pub(super) struct Building<'a> {
    db: &'a Database,
    oid: u32,
}

impl Drop for Building<'_> {
    fn drop(&mut self) {
        self.db.relfiles.of(self.oid).write().building.remove(&self.oid);
        self.db.relfiles.notify_built();
    }
}

/// Move table `oid` from kept to being laid out; None when it isn't kept (anymore).
fn start(db: &Database, oid: u32) -> Option<Building<'_>> {
    let mut rf = db.relfiles.of(oid).write();
    if !rf.kept.remove(&oid) {
        return None;
    }
    rf.building.insert(oid);
    Some(Building { db, oid })
}

/// Table `t`'s record, when it is of relfilenode, definition and fillfactor the catalog
/// `cat` gives it; with its hash.
fn record(db: &Database, cat: &Catalog, t: &Table) -> Option<(Restore, u64)> {
    let v = db.store.get(&key(t.oid), db.store.visible_ts()).ok()??;
    let node = cat.relfilenode(t.oid)?;
    let mut r = decode(&v, node, token(cat, t), heap_fillfactor(t))?;
    // Older tree images omitted NULLS NOT DISTINCT. Refresh both foreground and
    // background restoration from the catalog that owns the option.
    for (&oid, tree) in &mut r.btrees {
        if let Some(ix) = cat.indexes.get(&oid) {
            tree.opts.nulls_not_distinct = ix.nulls_not_distinct;
        }
    }
    Some((r, hash(&v)))
}

/// What was kept for `t` at the last clean close, when it's still valid and has every btree
/// index in `need`. Taken once: a later layout reads the rows.
pub(super) fn take<'a>(t: &Table, need: &[&BoundIndex], ctx: &'a ExecCtx) -> Take<'a> {
    let db = &ctx.sess.db;
    let Some(building) = start(db, t.oid) else {
        return if db.relfiles.of(t.oid).read().building.contains(&t.oid) { Take::Wait } else { Take::None };
    };
    let Some((r, h)) = record(db, &ctx.catalog(), t) else {
        return Take::None;
    };
    if need.iter().any(|b| !r.btrees.contains_key(&b.index.oid)) {
        return Take::None;
    }
    db.relfiles.of(t.oid).write().kept_hash.insert(t.oid, h);
    Take::Restore(r, building)
}

/// Kept table `t`'s sizes from its record's header (`Sizes`), while its models aren't laid
/// out: what the planner reads of it without waiting for the layout (a big table's takes
/// a quarter of a second). Read once, from the store; None once its models are there, or
/// when the record has no sizes or isn't valid for `t` any more.
pub(super) fn sizes(db: &Database, cat: &Catalog, t: &Table) -> Option<Sizes> {
    let pending = |rf: &RelFiles| !rf.heaps.contains_key(&t.oid) && (rf.kept.contains(&t.oid) || rf.building.contains(&t.oid));
    {
        let rf = db.relfiles.of(t.oid).read();
        if !pending(&rf) {
            return None;
        }
        if let Some(s) = rf.kept_sizes.get(&t.oid) {
            return s.clone();
        }
    }
    let v = db.store.get(&key(t.oid), db.store.visible_ts()).ok()??;
    let s = read_sizes(&v, cat.relfilenode(t.oid)?, token(cat, t)).0;
    let mut rf = db.relfiles.of(t.oid).write();
    if !pending(&rf) {
        return None;
    }
    rf.kept_sizes.insert(t.oid, s.clone());
    s
}

/// The sizes `sizes` found for table `oid`, without reading anything (None: none read).
pub(super) fn sizes_read(rf: &RelFiles, oid: u32) -> Option<&Sizes> {
    if rf.heaps.contains_key(&oid) {
        return None;
    }
    rf.kept_sizes.get(&oid)?.as_ref()
}

/// The models of kept table `oid` are laid out (`rf` has them): its header's sizes go, and
/// (debug builds) must be the models'.
pub(super) fn laid_out(rf: &mut RelFiles, oid: u32) {
    let Some(s) = rf.kept_sizes.remove(&oid).flatten() else { return };
    if cfg!(debug_assertions) {
        let heap = rf.heaps.get(&oid).map(Heap::nblocks);
        debug_assert_eq!(heap, Some(s.heap), "kept heap pages of {oid}");
        for &(ix, pages, level) in &s.btrees {
            let bt = rf.btrees.get(&ix).map(|b| (b.nblocks(), b.height()));
            debug_assert!(bt.is_none_or(|b| b == (pages, level)), "kept btree sizes of {ix}");
        }
    }
}

/// Lay every kept table's models out in the background as the database opens, so a
/// table's first statement usually finds them: one the builder hasn't reached yet is laid
/// out by its first user (`ensure`), who waits only while the builder is laying out that
/// very table. Nothing waits for the builder otherwise.
pub fn build_in_background(db: Arc<Database>) {
    let mut oids: Vec<u32> = db.relfiles.all().flat_map(|s| s.read().kept.iter().copied().collect::<Vec<_>>()).collect();
    if oids.is_empty() {
        return;
    }
    oids.sort_unstable();
    let spawned = std::thread::Builder::new().name("roll-models".into()).spawn(move || {
        for oid in oids {
            if let Some(building) = start(&db, oid) {
                lay_out(&db, oid, building);
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "laying out kept models in the background failed to start");
    }
}

/// `ensure`'s layout of a kept table, by the background builder (the database's committed
/// catalog; no prepared transaction has written to it, or it wouldn't have been kept).
fn lay_out(db: &Database, oid: u32, building: Building<'_>) {
    let cat = db.catalog();
    let Some(t) = cat.tables.get(&oid) else { return };
    let Some((r, h)) = record(db, &cat, t) else { return };
    let (mut heap, moved, _) = r.rows.restored(heap_fillfactor(t));
    if moved || heap.read_shape(&r.shape).is_none() {
        return;
    }
    let mut rf = db.relfiles.of(oid).write();
    // (Not if the table was dropped, truncated or rewritten meanwhile: `forget`.)
    if !rf.building.contains(&oid) || rf.heaps.contains_key(&oid) {
        return;
    }
    if r.exts.iter().any(|(_, e)| !e.is_empty()) {
        mark_tracked(db.instance, oid);
    }
    rf.heaps.insert(oid, heap);
    rebuild_toast(&mut rf, t, r.exts);
    for (ix, tree) in r.btrees {
        rf.btrees.insert(ix, tree);
    }
    rf.kept_hash.insert(oid, h);
    laid_out(&mut rf, oid);
    drop(rf);
    drop(building);
}

fn hash(b: &[u8]) -> u64 {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.write(b);
    h.finish()
}

/// The database is closing cleanly: keep its tables' models for its next open.
pub fn persist(db: &Database) {
    if let Err(e) = persist_inner(db) {
        tracing::warn!(error = %e, "keeping the heap models failed");
    }
}

fn persist_inner(db: &Database) -> Result<()> {
    let cat = db.catalog();
    let xids = db.xids();
    let v = Vis::new(&xids);
    // (One compression context for every record.)
    let mut zc = zstd::bulk::Compressor::new(1).map_err(|e| crate::error::internal(format!("zstd: {e}")))?;
    let mut writes: Vec<(Vec<u8>, Option<bytes::Bytes>)> = Vec::new();
    let mut keep: Vec<u32> = Vec::new();
    let mut stored: HashSet<u32> = HashSet::new();
    for shard in db.relfiles.all() {
        let rf = shard.read();
        // (Those still being laid out keep their records too.)
        keep.extend(rf.kept.iter().chain(&rf.building).copied().filter(|o| cat.tables.contains_key(o)));
        stored.extend(rf.kept_stored.iter().copied());
        for (&oid, h) in &rf.heaps {
            let Some(t) = cat.tables.get(&oid) else { continue };
            let Some(node) = cat.relfilenode(oid).filter(|n| *n != 0) else { continue };
            if t.temporary || rf.untracked.contains(&oid) {
                continue;
            }
            let start = index_prefix(oid, 0);
            if !db.twophase().writes_in(db.oid, &start, &prefix_end(&start))?.is_empty() {
                continue;
            }
            let Some(b) = encode(&rf, &cat, t, h, node, &v, &mut zc) else { continue };
            keep.push(oid);
            if !rf.kept_stored.contains(&oid) || rf.kept_hash.get(&oid) != Some(&hash(&b)) {
                writes.push((key(oid), Some(bytes::Bytes::from(b))));
            }
        }
    }
    keep.sort_unstable();
    for oid in stored {
        if keep.binary_search(&oid).is_err() {
            writes.push((key(oid), None));
        }
    }
    // (The marker last, listing the tables kept: what is kept is valid only with it.)
    let marker: Vec<u8> = keep.iter().flat_map(|o| o.to_be_bytes()).collect();
    writes.push((MARKER.to_vec(), Some(bytes::Bytes::from(marker))));
    db.store.commit(writes, |_| Ok(()))?;
    Ok(())
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// `b` after the prefix it shares with `prev`.
fn put_suffix(out: &mut Vec<u8>, prev: &[u8], b: &[u8]) {
    let shared = prev.iter().zip(b).take_while(|(x, y)| x == y).count();
    put(out, shared as u64);
    put(out, (b.len() - shared) as u64);
    out.extend_from_slice(&b[shared..]);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Table `t`'s record: its live rows as `ensure` would read them (in TID order), with the
/// keys its btree indexes have for them. None when its model can't say (rows not at their
/// stored TIDs).
fn encode(rf: &RelFiles, cat: &Catalog, t: &Table, h: &Heap, node: u32, v: &Vis, zc: &mut zstd::bulk::Compressor) -> Option<Vec<u8>> {
    if h.unstamped {
        return None;
    }
    let rows = h.committed_rows(v);
    if rows.windows(2).any(|w| w[0].0 == w[1].0) {
        return None;
    }
    let mut out = vec![VERSION];
    out.extend_from_slice(&node.to_le_bytes());
    out.extend_from_slice(&token(cat, t).to_le_bytes());
    put(&mut out, heap_fillfactor(t) as u64);
    put(&mut out, rows.len() as u64);
    let toast = rf.toast.get(&t.oid);
    let (mut pk, mut pb): (&[u8], u32) = (&[], 0);
    for &(tid, k, len) in &rows {
        put(&mut out, zigzag(tid.0 as i64 - pb as i64));
        put(&mut out, tid.1 as u64);
        put(&mut out, len as u64);
        put_suffix(&mut out, pk, k);
        (pk, pb) = (k, tid.0);
        let exts = toast.and_then(|x| x.rows.get(k)).map_or(&[][..], |e| &e[..]);
        put(&mut out, exts.len() as u64);
        for e in exts {
            put(&mut out, e.col as u64);
            put(&mut out, e.bytes as u64);
            put(&mut out, e.sig);
        }
    }
    // (The pages' line pointers as they are: index entries keep their TIDs.)
    let mut shape = Vec::new();
    h.write_shape(&mut shape, &|tid| rows.binary_search_by_key(&tid, |r| r.0).is_ok());
    put(&mut out, shape.len() as u64);
    out.extend_from_slice(&shape);
    let ixs: Vec<u32> = index_oids(t).into_iter().filter(|o| rf.btrees.contains_key(o)).collect();
    put(&mut out, ixs.len() as u64);
    let mut ixs_sizes = Vec::with_capacity(ixs.len());
    for oid in ixs {
        out.extend_from_slice(&oid.to_le_bytes());
        let bt = &rf.btrees[&oid];
        bt.write_image(&mut out);
        ixs_sizes.push((oid, bt.nblocks(), bt.height()));
    }
    // (A small record as it is, a bigger one compressed: a tag says which.)
    let mut z = sized_header(t, h, node, cat, &ixs_sizes);
    if out.len() < 1024 {
        z.push(RAW);
        z.extend_from_slice(&out);
        return Some(z);
    }
    z.push(ZSTD);
    z.extend_from_slice(&zc.compress(&out).ok()?);
    Some(z)
}

/// The sizes a planner reads of a kept table before its models are laid out (`sizes`): its
/// heap's pages, and each btree's pages and root level. Kept only for a heap whose pages are
/// all visible with none prunable: reads of it then change nothing in its models (no page
/// to prune, no index entry to mark dead), so they needn't wait for the layout.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sizes {
    pub heap: u32,
    /// (index, pages, root level)
    pub btrees: Vec<(u32, u32, u32)>,
}

/// A record's first byte when the sizes come first (`SIZED`, the relfilenode, the
/// definition's token, then the sizes), before the record as it was (`RAW` / `ZSTD`): the
/// sizes read without decompressing the rows. (Older builds don't know the tag: they lay
/// the table out from its rows.)
const SIZED: u8 = 0xf2;

fn sized_header(t: &Table, h: &Heap, node: u32, cat: &Catalog, btrees: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    if h.visibility_watch().is_none() || h.any_prunable() {
        return out;
    }
    out.push(SIZED);
    out.extend_from_slice(&node.to_le_bytes());
    out.extend_from_slice(&token(cat, t).to_le_bytes());
    put(&mut out, h.nblocks() as u64);
    put(&mut out, btrees.len() as u64);
    for &(oid, pages, level) in btrees {
        out.extend_from_slice(&oid.to_le_bytes());
        put(&mut out, pages as u64);
        put(&mut out, level as u64);
    }
    out
}

/// A record's sizes (`Sizes`), when it has them for relfilenode `node` and definition
/// `token`; and the record after them.
fn read_sizes(v: &[u8], node: u32, token: u64) -> (Option<Sizes>, &[u8]) {
    let Some((&SIZED, rest)) = v.split_first() else { return (None, v) };
    let mut r = Rd(rest);
    let mut read = || -> Option<Sizes> {
        let same = u32::from_le_bytes(r.take(4)?.try_into().ok()?) == node
            && u64::from_le_bytes(r.take(8)?.try_into().ok()?) == token;
        let heap = u32::try_from(r.get()?).ok()?;
        let mut btrees = Vec::new();
        for _ in 0..r.get()? {
            let oid = u32::from_le_bytes(r.take(4)?.try_into().ok()?);
            btrees.push((oid, u32::try_from(r.get()?).ok()?, u32::try_from(r.get()?).ok()?));
        }
        same.then_some(Sizes { heap, btrees })
    };
    let s = read();
    (s, r.0)
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn get(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.take(1)?[0];
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }
    fn delta(&mut self, prev: u64) -> Option<u64> {
        let z = self.get()?;
        let d = (z >> 1) as i64 ^ -((z & 1) as i64);
        u64::try_from(prev as i64 + d).ok()
    }
    /// A key stored as its shared prefix with the previous key (the last `prev` bytes of
    /// `keys`) and the rest, appended to `keys`.
    fn suffix_into(&mut self, keys: &mut Vec<u8>, prev: usize) -> Option<()> {
        let shared = self.get()? as usize;
        let n = self.get()? as usize;
        if shared > prev {
            return None;
        }
        let start = keys.len() - prev;
        keys.extend_from_within(start..start + shared);
        keys.extend_from_slice(self.take(n)?);
        Some(())
    }
}

/// A record `encode` wrote, when it is of relfilenode `node`, definition `token` and
/// fillfactor `ff`. (The definition's hash covers the btrees' options kept with them.)
fn decode(v: &[u8], node: u32, token: u64, ff: usize) -> Option<Restore> {
    let raw;
    let (_, v) = read_sizes(v, node, token);
    let body = match v.split_first()? {
        (&RAW, b) => b,
        (&ZSTD, b) => {
            raw = zstd::stream::decode_all(b).ok()?;
            &raw[..]
        }
        _ => return None,
    };
    let mut r = Rd(body);
    if r.take(1)? != [VERSION]
        || u32::from_le_bytes(r.take(4)?.try_into().ok()?) != node
        || u64::from_le_bytes(r.take(8)?.try_into().ok()?) != token
        || r.get()? != ff as u64
    {
        return None;
    }
    let n = r.get()? as usize;
    let mut rows = KeptRows { keys: Vec::new(), rows: Vec::with_capacity(n) };
    let mut exts = Vec::new();
    let mut pb = 0u64;
    let mut prev = 0;
    for i in 0..n {
        pb = r.delta(pb)?;
        let off = u16::try_from(r.get()?).ok()?;
        let len = u32::try_from(r.get()?).ok()?;
        let start = rows.keys.len();
        r.suffix_into(&mut rows.keys, prev)?;
        prev = rows.keys.len() - start;
        let end = u32::try_from(rows.keys.len()).ok()?;
        rows.rows.push((end, len, (u32::try_from(pb).ok()?, off)));
        let mut e = Vec::new();
        for _ in 0..r.get()? {
            let (col, bytes, sig) = (r.get()? as usize, r.get()? as usize, r.get()?);
            e.push(NewExt { col, bytes, sig });
        }
        if !e.is_empty() {
            exts.push((rows.key(i).to_vec(), e));
        }
    }
    let n = r.get()? as usize;
    let shape = r.take(n)?.to_vec();
    let mut btrees = HashMap::new();
    for _ in 0..r.get()? {
        let oid = u32::from_le_bytes(r.take(4)?.try_into().ok()?);
        btrees.insert(oid, Btree::read_image(&mut r.0)?);
    }
    r.0.is_empty().then_some(Restore { rows, exts, shape, btrees })
}
