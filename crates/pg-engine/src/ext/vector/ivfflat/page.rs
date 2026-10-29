//! The index's pages, as pgvector lays them out (`ivfflat.h`, `ivfutils.c`): the meta page
//! (block 0), the list pages from block 1 (an item per list: its start and insert pages and
//! its center), and each list's chain of entry pages (an index tuple per row). Each page is
//! modelled to the byte where it decides anything: the page header, line pointers, MAXALIGNed
//! items and the special space, so that PageGetFreeSpace, and with it where a list's items
//! go and how many blocks the index has, is PostgreSQL's.
//!
//! Each page is a record of the index's store, keyed by its block number and written as
//! PostgreSQL writes the page; a process-wide cache keeps each index's pages in memory,
//! shared by the sessions.

use super::types::Kind;
use crate::Result;
use crate::ext::index_am::AmStore;
use crate::types::Value;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// BLCKSZ.
pub(super) const BLCKSZ: usize = 8192;
/// SizeOfPageHeaderData.
const PAGE_HEADER: usize = 24;
/// MAXALIGN(sizeof(IvfflatPageOpaqueData)).
const SPECIAL: usize = 8;
/// sizeof(ItemIdData).
const ITEM_ID: usize = 4;
/// InvalidBlockNumber.
pub(super) const INVALID: u32 = u32::MAX;
/// IVFFLAT_HEAD_BLKNO: the first list page.
pub(super) const HEAD_BLKNO: u32 = 1;

pub(super) fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

/// `PageGetFreeSpace` of a page holding `n` items of `used` bytes.
pub(super) fn free_space(n: usize, used: usize) -> usize {
    let lower = PAGE_HEADER + ITEM_ID * n;
    let upper = BLCKSZ - SPECIAL - used;
    let space = upper as isize - lower as isize;
    if space < ITEM_ID as isize { 0 } else { space as usize - ITEM_ID }
}

/// TOAST_INDEX_TARGET: MaxHeapTupleSize / 16.
const TOAST_INDEX_TARGET: usize = (BLCKSZ - 32) / 16;

/// `MAXALIGN(IndexTupleSize(itup))` of the tuple `index_form_tuple` makes of `v`: the 8-byte
/// header, then the value with a 1-byte header when it is short enough (the types aren't
/// PLAIN), else int-aligned with its 4-byte one; a bit string over TOAST_INDEX_TARGET
/// (storage EXTENDED) compressed in line when that saves enough (`lz4`:
/// default_toast_compression).
pub(super) fn tuple_size(kind: Kind, v: &Value, lz4: bool) -> usize {
    let vs = kind.varsize(v);
    let data = if vs - 3 <= 127 {
        vs - 3
    } else if kind == Kind::Bit && vs > TOAST_INDEX_TARGET {
        compressed(v, vs, lz4).unwrap_or(vs)
    } else {
        vs
    };
    maxalign(8 + data)
}

/// The size of a bit string `toast_compress_datum` compresses (None: it doesn't).
fn compressed(v: &Value, vs: usize, lz4: bool) -> Option<usize> {
    let Value::Bits(n, b) = v else { return None };
    // VARDATA of a varbit: its length, then the bytes.
    let mut data = Vec::with_capacity(vs - 4);
    data.extend_from_slice(&(*n as i32).to_le_bytes());
    data.extend_from_slice(&b[..(*n as usize).div_ceil(8).min(b.len())]);
    data.resize(vs - 4, 0);
    let len = if lz4 {
        crate::catalog::lz4::compressed_len(&data).filter(|l| *l <= data.len())?
    } else {
        crate::catalog::pglz::compressed_len(&data)?
    };
    // VARHDRSZ_COMPRESSED; "we insist on a savings of more than 2 bytes"
    (len + 8 < data.len() - 2).then_some(len + 8)
}

/// An index tuple: the row (its data key, PostgreSQL's heap TID) and the indexed value
/// (normalized when the class normalizes; `orig` then keeps the row's own, which the engine
/// matches rows against).
#[derive(Clone)]
pub(super) struct Tuple {
    pub dkey: Arc<[u8]>,
    pub val: Value,
    pub orig: Option<Value>,
    /// MAXALIGN(IndexTupleSize).
    pub size: u32,
}

impl Tuple {
    /// The row's value of the indexed column.
    pub(super) fn row_value(&self) -> &Value {
        self.orig.as_ref().unwrap_or(&self.val)
    }
}

/// `IvfflatListData`.
#[derive(Clone)]
pub(super) struct List {
    pub start: u32,
    pub insert: u32,
    pub center: Value,
}

/// A page of the index.
pub(super) enum Page {
    /// `IvfflatMetaPageData` (with the type, for reading the records back).
    Meta { dims: i32, lists: i32, kind: Kind },
    /// A list page: its lists in offset order and the next list page.
    Lists { next: u32, lists: Vec<List> },
    /// An entry page: its tuples in offset order (shared with the scans reading them, copied
    /// when changed while one does), the bytes they take and the next page of the list.
    Entries { next: u32, tuples: Arc<Vec<Tuple>>, used: usize },
}

impl Page {
    /// A new entry page (`IvfflatInitPage`).
    pub(super) fn entries() -> Page {
        Page::Entries { next: INVALID, tuples: Arc::new(Vec::new()), used: 0 }
    }

    pub(super) fn next(&self) -> u32 {
        match self {
            Page::Meta { .. } => INVALID,
            Page::Lists { next, .. } | Page::Entries { next, .. } => *next,
        }
    }

    pub(super) fn set_next(&mut self, n: u32) {
        if let Page::Lists { next, .. } | Page::Entries { next, .. } = self {
            *next = n;
        }
    }

    /// An entry page's tuples (none for another page).
    pub(super) fn tuples(&self) -> &[Tuple] {
        match self {
            Page::Entries { tuples, .. } => tuples.as_slice(),
            _ => &[],
        }
    }

    /// An entry page's tuples as they are now, for a scan to keep (None for another page).
    pub(super) fn shared_tuples(&self) -> Option<Arc<Vec<Tuple>>> {
        match self {
            Page::Entries { tuples, .. } => Some(tuples.clone()),
            _ => None,
        }
    }

    /// PageGetFreeSpace of an entry page.
    pub(super) fn free(&self) -> usize {
        match self {
            Page::Entries { tuples, used, .. } => free_space(tuples.len(), *used),
            _ => 0,
        }
    }

    /// PageAddItem of an entry page (the caller checked the space).
    pub(super) fn add(&mut self, t: Tuple) {
        if let Page::Entries { tuples, used, .. } = self {
            *used += t.size as usize;
            Arc::make_mut(tuples).push(t);
        }
    }
}

/// An index: its pages by block number.
pub(super) struct Index {
    pub pages: Vec<Page>,
}

/// Where a list is: its list page and its offset there (from 0).
#[derive(Clone, Copy, Debug)]
pub(super) struct ListInfo {
    pub blkno: u32,
    pub off: usize,
}

impl Index {
    /// `IvfflatGetMetaPageInfo`: (lists, dimensions, type).
    pub(super) fn meta(&self) -> Result<(i32, i32, Kind)> {
        match self.pages.first() {
            Some(Page::Meta { dims, lists, kind }) => Ok((*lists, *dims, *kind)),
            _ => Err(crate::error::internal("ivfflat index is not valid")),
        }
    }

    /// The lists in list page order, with where each is.
    pub(super) fn lists(&self) -> impl Iterator<Item = (ListInfo, &List)> + '_ {
        let mut blkno = HEAD_BLKNO;
        std::iter::from_fn(move || {
            if blkno == INVALID {
                return None;
            }
            let b = blkno;
            let Some(Page::Lists { next, lists }) = self.pages.get(b as usize) else {
                blkno = INVALID;
                return None;
            };
            blkno = *next;
            Some(lists.iter().enumerate().map(move |(off, l)| (ListInfo { blkno: b, off }, l)))
        })
        .flatten()
    }

    pub(super) fn list_mut(&mut self, at: ListInfo) -> &mut List {
        match &mut self.pages[at.blkno as usize] {
            Page::Lists { lists, .. } => &mut lists[at.off],
            _ => unreachable!("not a list page"),
        }
    }

    /// `IvfflatNewBuffer`: a new page at the end of the relation (P_NEW).
    pub(super) fn new_page(&mut self, p: Page) -> u32 {
        self.pages.push(p);
        (self.pages.len() - 1) as u32
    }

    /// `IvfflatUpdateList`: the list's insert page (unless lower than the insert page the
    /// caller started from, which a VACUUM may have lowered meanwhile) and start page;
    /// whether it changed.
    pub(super) fn update_list(&mut self, at: ListInfo, insert: u32, original_insert: u32, start: u32) -> bool {
        let list = self.list_mut(at);
        let mut changed = false;
        if insert != INVALID && insert != list.insert && (original_insert == INVALID || insert >= original_insert) {
            list.insert = insert;
            changed = true;
        }
        if start != INVALID && start != list.start {
            list.start = start;
            changed = true;
        }
        changed
    }

    /// The pages of the store's records (None: it has none).
    pub(super) fn load(store: &AmStore) -> Result<Option<Index>> {
        let recs = store.load()?;
        if recs.is_empty() {
            return Ok(None);
        }
        let mut pages = Vec::with_capacity(recs.len());
        for (k, v) in recs {
            let blkno = k.as_slice().try_into().map(u32::from_be_bytes).map_err(|_| invalid())?;
            if blkno as usize != pages.len() {
                return Err(invalid());
            }
            pages.push(decode(&v)?);
        }
        Ok(Some(Index { pages }))
    }

    /// Write pages `blocks` to the store (as PostgreSQL writes those pages: at once).
    pub(super) fn save(&self, store: &AmStore, blocks: impl IntoIterator<Item = u32>) -> Result<()> {
        let mut batch = Vec::new();
        let mut bytes = 0;
        for b in blocks {
            let rec = encode(&self.pages[b as usize]);
            bytes += rec.len();
            batch.push((b.to_be_bytes().to_vec(), Some(rec)));
            // (A build's pages go in commits of a few megabytes.)
            if bytes >= 4 << 20 {
                store.write(std::mem::take(&mut batch))?;
                bytes = 0;
            }
        }
        store.write(batch)
    }
}

fn invalid() -> crate::Error {
    crate::error::internal("ivfflat index is not valid")
}

// ── records ──

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Ext(k, b) => {
            out.push(0);
            out.extend_from_slice(&k.to_le_bytes());
            put_bytes(out, b);
        }
        Value::Bits(n, b) => {
            out.push(1);
            put_u32(out, *n);
            put_bytes(out, b);
        }
        _ => out.push(2),
    }
}

/// A page's record.
fn encode(p: &Page) -> Vec<u8> {
    let mut out = Vec::new();
    match p {
        Page::Meta { dims, lists, kind } => {
            out.push(0);
            put_u32(&mut out, *dims as u32);
            put_u32(&mut out, *lists as u32);
            out.push(kind.tag());
        }
        Page::Lists { next, lists } => {
            out.push(1);
            put_u32(&mut out, *next);
            put_u32(&mut out, lists.len() as u32);
            for l in lists {
                put_u32(&mut out, l.start);
                put_u32(&mut out, l.insert);
                put_value(&mut out, &l.center);
            }
        }
        Page::Entries { next, tuples, .. } => {
            out.push(2);
            put_u32(&mut out, *next);
            put_u32(&mut out, tuples.len() as u32);
            for t in tuples.iter() {
                put_bytes(&mut out, &t.dkey);
                put_u32(&mut out, t.size);
                put_value(&mut out, &t.val);
                match &t.orig {
                    Some(o) => put_value(&mut out, o),
                    None => out.push(3),
                }
            }
        }
    }
    out
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos + n).ok_or_else(invalid)?;
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// A value (None: a tuple without its row's own).
    fn value(&mut self) -> Result<Option<Value>> {
        Ok(match self.u8()? {
            0 => {
                let k = u16::from_le_bytes(self.take(2)?.try_into().unwrap());
                Some(Value::Ext(k, self.bytes()?.into()))
            }
            1 => {
                let n = self.u32()?;
                Some(Value::Bits(n, self.bytes()?.into()))
            }
            2 => Some(Value::Null),
            _ => None,
        })
    }
}

fn decode(b: &[u8]) -> Result<Page> {
    let mut r = Reader { b, pos: 0 };
    Ok(match r.u8()? {
        0 => Page::Meta { dims: r.u32()? as i32, lists: r.u32()? as i32, kind: Kind::from_tag(r.u8()?) },
        1 => {
            let next = r.u32()?;
            let n = r.u32()? as usize;
            let mut lists = Vec::with_capacity(n);
            for _ in 0..n {
                let (start, insert) = (r.u32()?, r.u32()?);
                lists.push(List { start, insert, center: r.value()?.ok_or_else(invalid)? });
            }
            Page::Lists { next, lists }
        }
        2 => {
            let next = r.u32()?;
            let n = r.u32()? as usize;
            let mut tuples = Vec::with_capacity(n);
            let mut used = 0;
            for _ in 0..n {
                let dkey: Arc<[u8]> = r.bytes()?.into();
                let size = r.u32()?;
                let val = r.value()?.ok_or_else(invalid)?;
                let orig = r.value()?;
                used += size as usize;
                tuples.push(Tuple { dkey, val, orig, size });
            }
            Page::Entries { next, tuples: Arc::new(tuples), used }
        }
        _ => return Err(invalid()),
    })
}

// ── the cache ──

/// An index's pages in memory (`index`: None when the store has no records).
pub(super) struct Slot {
    loaded: bool,
    pub index: Option<Index>,
}

type Cache = Mutex<HashMap<(u64, Vec<u8>), Arc<Mutex<Slot>>>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Run `f` over the index of `store` (read from its records the first time), locked: one
/// session at a time changes or reads it.
pub(super) fn with<R>(store: &AmStore, f: impl FnOnce(&mut Option<Index>) -> Result<R>) -> Result<R> {
    let slot = cache().lock().entry(store.id()).or_insert_with(|| Arc::new(Mutex::new(Slot { loaded: false, index: None }))).clone();
    let mut g = slot.lock();
    if !g.loaded {
        g.index = Index::load(store)?;
        g.loaded = true;
    }
    f(&mut g.index)
}

/// A built index becomes the one of `store`. (The index's other storage generations, before
/// a TRUNCATE or REINDEX gave it this one, leave the cache: a rollback reads them back.)
pub(super) fn install(store: &AmStore, index: Index) {
    let id = store.id();
    let mut c = cache().lock();
    let base = id.1.len().saturating_sub(4);
    c.retain(|k, _| !(k.0 == id.0 && k.1.len() == id.1.len() && k.1[..base] == id.1[..base] && k.1 != id.1));
    c.insert(id, Arc::new(Mutex::new(Slot { loaded: true, index: Some(index) })));
}
