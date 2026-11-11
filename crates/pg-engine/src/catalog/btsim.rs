//! A model of a btree index file as Postgres builds and grows it: a metapage, leaf pages
//! filled to the fillfactor by a sorted build (with deduplication into posting lists and
//! suffix-truncated high keys), page splits at the point nbtsplitloc.c would choose,
//! deduplication passes that put off splits, and new levels as the root splits.

use super::heapsim::{BLCKSZ, ITEM_ID, PAGE_HEADER, Tid, get_varint as get, maxalign, put_varint as put};
use std::cmp::Ordering;

mod leaf;
use leaf::Leaf;

/// BTPageOpaqueData, MAXALIGNed.
const SPECIAL: usize = 16;
/// Usable bytes on a btree page.
const PAGE_SPACE: usize = BLCKSZ - PAGE_HEADER - SPECIAL;
/// BTMaxItemSize.
const MAX_ITEM: usize = 2704;
const NONLEAF_FILLFACTOR: usize = 70;
const SINGLEVAL_FILLFACTOR: usize = 96;
/// A heap TID appended to a pivot tuple (MAXALIGN(sizeof(ItemPointerData))).
const PIVOT_TID: usize = 8;
/// A pivot tuple with no key attributes ("minus infinity").
const MINUS_INF: usize = 8;

/// An index tuple's key: each attribute's order-preserving encoding (key attributes,
/// then INCLUDE ones), and the tuple's size when only its first k attributes are kept.
/// One allocation (a model holds one per index entry of every modelled table): the
/// attribute and size counts, each attribute's end and each prefix's size as u16s, then
/// the attributes' bytes.
/// A thin pointer to the bytes, which say their own length (the counts and the last
/// attribute's end): a leaf item is 32 bytes and its key's bytes, nothing else. A clone
/// copies them (only pivots clone keys, a few per page split).
pub struct Key {
    ptr: std::ptr::NonNull<u8>,
}

// SAFETY: the bytes are owned and never change after `wrap`.
unsafe impl Send for Key {}
unsafe impl Sync for Key {}

impl Key {
    /// The length of the encoding at `p` (its header is valid: `wrap` checked it).
    fn len_at(p: *const u8) -> usize {
        // SAFETY: the header (counts, then the attributes' ends) is within the allocation.
        unsafe {
            let (natts, nsizes) = (*p as usize, *p.add(1) as usize);
            let data = 2 + 2 * (natts + nsizes);
            if natts == 0 {
                return data;
            }
            let at = p.add(2 + 2 * (natts - 1));
            data + u16::from_le_bytes([*at, *at.add(1)]) as usize
        }
    }

    fn layout(n: usize) -> std::alloc::Layout {
        std::alloc::Layout::from_size_align(n.max(1), 1).expect("key layout")
    }

    /// `buf` must be a well-formed encoding (`new`, or checked by `from_bytes`).
    fn wrap(buf: &[u8]) -> Key {
        debug_assert!(buf.len() >= 2 && Self::len_at(buf.as_ptr()) == buf.len());
        let layout = Self::layout(buf.len());
        // SAFETY: a non-zero-sized layout; the copy fills what was allocated.
        unsafe {
            let p = std::alloc::alloc(layout);
            let Some(ptr) = std::ptr::NonNull::new(p) else {
                std::alloc::handle_alloc_error(layout)
            };
            std::ptr::copy_nonoverlapping(buf.as_ptr(), p, buf.len());
            Key { ptr }
        }
    }

    /// Its encoding (`sizing`'s models kept across a restart store it as it is).
    pub fn as_bytes(&self) -> &[u8] {
        let p = self.ptr.as_ptr();
        // SAFETY: the allocation holds exactly the encoding, `len_at` long.
        unsafe { std::slice::from_raw_parts(p, Self::len_at(p)) }
    }

    /// A key from `as_bytes`, when they hold one.
    pub fn from_bytes(buf: &[u8]) -> Option<Key> {
        Self::well_formed(buf).then(|| Key::wrap(buf))
    }

    /// Whether `buf` holds exactly one key's encoding.
    fn well_formed(buf: &[u8]) -> bool {
        Self::check(buf).is_some()
    }

    fn check(buf: &[u8]) -> Option<()> {
        let (&natts, &nsizes) = (buf.first()?, buf.get(1)?);
        let data = 2 + 2 * (natts as usize + nsizes as usize);
        let at = |i: usize| buf.get(2 + 2 * i..4 + 2 * i).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
        let mut prev = 0;
        for a in 0..natts as usize {
            let end = at(a)?;
            if end < prev {
                return None;
            }
            prev = end;
        }
        (data <= buf.len() && data + prev == buf.len()).then_some(())
    }

    /// `atts`: per-attribute encodings; `sizes[k]`: the MAXALIGNed index tuple size
    /// holding the first k+1 attributes.
    pub fn new(atts: &[Vec<u8>], sizes: &[usize]) -> Key {
        debug_assert_eq!(atts.len(), sizes.len());
        Key::build_in(&mut Vec::new(), atts.len(), |i, buf| {
            buf.extend_from_slice(&atts[i]);
            sizes[i]
        })
    }
}

impl Key {
    /// A key of `natts` attributes encoded in `buf` (scratch space, cleared first; the key
    /// is the only allocation): `att(i, buf)` appends attribute i's encoding to `buf` and
    /// returns the MAXALIGNed size of an index tuple holding attributes 0..=i (`new`'s
    /// `sizes`).
    ///
    /// An attribute's encoding longer than `ATT_MAX` keeps its first bytes and a hash of the
    /// rest (a value that long is compressed in a PostgreSQL index tuple, whose size `att`
    /// gives as it is): the key's bytes stay within its u16 offsets for any number of
    /// attributes, equal values stay equal, and only values alike in their first
    /// `ATT_MAX` bytes order by hash. Sizes past u16 (a tuple PostgreSQL rejects) are capped.
    pub fn build_in(buf: &mut Vec<u8>, natts: usize, mut att: impl FnMut(usize, &mut Vec<u8>) -> usize) -> Key {
        let natts = natts.min(u8::MAX as usize);
        let head = 2 + 4 * natts;
        buf.clear();
        buf.resize(head, 0);
        (buf[0], buf[1]) = (natts as u8, natts as u8);
        for i in 0..natts {
            let start = buf.len();
            let size = att(i, buf).min(u16::MAX as usize) as u16;
            if buf.len() - start > ATT_MAX {
                use std::hash::Hasher;
                // (SipHash with fixed keys: the same in every process, kept images too.)
                let mut h = std::collections::hash_map::DefaultHasher::new();
                h.write(&buf[start + ATT_MAX - 8..]);
                buf.truncate(start + ATT_MAX - 8);
                buf.extend_from_slice(&h.finish().to_be_bytes());
            }
            let end = (buf.len() - head) as u16;
            buf[2 + 2 * i..4 + 2 * i].copy_from_slice(&end.to_le_bytes());
            buf[2 + 2 * (natts + i)..4 + 2 * (natts + i)].copy_from_slice(&size.to_le_bytes());
        }
        Key::wrap(buf)
    }
}

/// The bytes a key keeps of one attribute's encoding (`Key::build_in`): INDEX_MAX_KEYS (32)
/// of them fit the key's u16 offsets.
const ATT_MAX: usize = 1800;

impl std::ops::Deref for Key {
    type Target = KeyBytes;
    fn deref(&self) -> &KeyBytes {
        KeyBytes::of(self.as_bytes())
    }
}

/// A key's encoding, borrowed (`Key`'s, or one packed on a leaf page: `leaf::Leaf`).
#[repr(transparent)]
pub struct KeyBytes([u8]);

impl KeyBytes {
    /// `b` must be a well-formed encoding.
    pub(super) fn of(b: &[u8]) -> &KeyBytes {
        // SAFETY: `KeyBytes` is a transparent wrapper of `[u8]`.
        unsafe { &*(b as *const [u8] as *const KeyBytes) }
    }

    /// The length of the encoding `b` starts with (its header must be there).
    pub(super) fn len_of(b: &[u8]) -> usize {
        let (natts, nsizes) = (b[0] as usize, b[1] as usize);
        let data = 2 + 2 * (natts + nsizes);
        if natts == 0 {
            return data;
        }
        let at = 2 + 2 * (natts - 1);
        data + u16::from_le_bytes([b[at], b[at + 1]]) as usize
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn byte(&self, i: usize) -> u8 {
        self.0[i]
    }

    /// An owned copy.
    pub fn to_key(&self) -> Key {
        Key::wrap(&self.0)
    }

    fn u16_at(&self, i: usize) -> usize {
        u16::from_le_bytes([self.byte(2 + 2 * i), self.byte(3 + 2 * i)]) as usize
    }

    fn natts(&self) -> usize {
        self.byte(0) as usize
    }

    fn nsizes(&self) -> usize {
        self.byte(1) as usize
    }

    /// Where the attributes' bytes start.
    fn data(&self) -> usize {
        2 + 2 * (self.natts() + self.nsizes())
    }

    /// Where attribute `a`'s bytes end (from `data`).
    fn end(&self, a: usize) -> usize {
        self.u16_at(a)
    }

    fn seg(&self, a: usize) -> &[u8] {
        let start = if a == 0 { 0 } else { self.end(a - 1) };
        let d = self.data();
        &self.as_bytes()[d + start..d + self.end(a)]
    }

    /// The whole tuple's size (no posting list).
    fn size(&self) -> usize {
        match self.nsizes() {
            0 => 8,
            n => self.u16_at(self.natts() + n - 1),
        }
    }

    /// The size of a tuple keeping only the first `natts` attributes.
    fn prefix_size(&self, natts: usize) -> usize {
        if natts == 0 {
            MINUS_INF
        } else {
            self.u16_at(self.natts() + (natts - 1).min(self.nsizes() - 1))
        }
    }

    /// The key attributes' encodings, as an index entry's storage key has them.
    fn key_bytes(&self, nkey: usize) -> &[u8] {
        let n = nkey.min(self.natts());
        let d = self.data();
        &self.as_bytes()[d..d + if n == 0 { 0 } else { self.end(n - 1) }]
    }

    fn cmp_keys(&self, o: &KeyBytes, nkey: usize) -> Ordering {
        for a in 0..nkey {
            match self.seg(a).cmp(o.seg(a)) {
                Ordering::Equal => {}
                x => return x,
            }
        }
        Ordering::Equal
    }

    /// NULL's tag is 0xff, or its inverse for DESC / NULLS FIRST. Numeric zero,
    /// infinities and NaN also have one-byte encodings, with different tags.
    fn has_null(&self, nkey: usize) -> bool {
        (0..nkey.min(self.natts())).any(|a| matches!(self.seg(a), [0] | [0xff]))
    }

    /// _bt_keep_natts_fast: the first key attribute that differs (1-based), or nkey + 1.
    fn keep_natts(&self, o: &KeyBytes, nkey: usize) -> usize {
        for a in 0..nkey {
            if self.seg(a) != o.seg(a) {
                return a + 1;
            }
        }
        nkey + 1
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        let n = Self::len_at(self.ptr.as_ptr());
        // SAFETY: allocated by `wrap` with this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), Self::layout(n)) }
    }
}

impl Clone for Key {
    fn clone(&self) -> Key {
        Key::wrap(self.as_bytes())
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({:?})", self.as_bytes())
    }
}

/// An item's heap TIDs, ascending: one inline (the common case, no allocation), or a
/// posting list's.
#[derive(Clone, Debug)]
enum Tids {
    One(Tid),
    Many(Box<[Tid]>),
}

impl std::ops::Deref for Tids {
    type Target = [Tid];
    fn deref(&self) -> &[Tid] {
        match self {
            Tids::One(t) => std::slice::from_ref(t),
            Tids::Many(v) => v,
        }
    }
}

impl From<Vec<Tid>> for Tids {
    fn from(v: Vec<Tid>) -> Tids {
        match v[..] {
            [t] => Tids::One(t),
            _ => Tids::Many(v.into()),
        }
    }
}

impl Tids {
    fn retain(&mut self, f: impl FnMut(&Tid) -> bool) {
        let mut v = self.to_vec();
        v.retain(f);
        *self = v.into();
    }

    fn into_vec(self) -> Vec<Tid> {
        match self {
            Tids::One(t) => vec![t],
            Tids::Many(v) => v.into(),
        }
    }
}

#[derive(Clone, Debug)]
struct Item {
    key: Key,
    /// Heap TIDs, ascending (more than one: a posting list tuple).
    tids: Tids,
    /// LP_DEAD: an index scan found every heap tuple it points at dead to everyone
    /// (_bt_killitems).
    dead: bool,
}

impl Item {
    fn plain(key: Key, tid: Tid) -> Item {
        Item {
            key,
            tids: Tids::One(tid),
            dead: false,
        }
    }

    fn posting(key: Key, tids: impl Into<Tids>) -> Item {
        Item {
            key,
            tids: tids.into(),
            dead: false,
        }
    }

    /// The tuple's size: its key's, a posting list's TIDs with it (a function of the two,
    /// so not kept: a model holds an item per index entry).
    fn size(&self) -> usize {
        if self.tids.len() > 1 {
            maxalign(self.key.size() + 6 * self.tids.len())
        } else {
            self.key.size()
        }
    }

    fn posting_bytes(&self) -> usize {
        self.size() - self.key.size()
    }
}

#[derive(Clone, Debug)]
struct Pivot {
    key: Option<Key>,
    natts: usize,
    tid: Option<Tid>,
    size: usize,
}

impl Pivot {
    fn minus_inf() -> Pivot {
        Pivot {
            key: None,
            natts: 0,
            tid: None,
            size: MINUS_INF,
        }
    }

    /// Below `lo` (minus infinity is below everything), comparing its kept attributes.
    fn below(&self, lo: &[u8], nkey: usize) -> bool {
        match &self.key {
            None => true,
            Some(k) => k.key_bytes(self.natts.min(nkey)) < lo,
        }
    }
}

/// Which child of an inner page's pivots (in key order) a descent to `lo` takes: the last
/// one below it (else the first). A binary search: inner pages hold hundreds of pivots.
fn child_for(inner: &[(Pivot, u32)], lo: &[u8], nkey: usize) -> usize {
    let i = inner.partition_point(|(pv, _)| pv.below(lo, nkey)).saturating_sub(1);
    debug_assert_eq!(
        i,
        inner.iter().rposition(|(pv, _)| pv.below(lo, nkey)).unwrap_or(0),
        "pivots out of order"
    );
    i
}

/// Whether a leaf item is past model key `lo` for a scan: some row key `lo` goes on with
/// puts it there (a storage key is the entry's key attributes, then the row's key).
fn above(k: &[u8], lo: &[u8]) -> bool {
    let m = k.len().min(lo.len());
    k[..m] >= lo[..m]
}

/// The first of a leaf's items (in key order) past `lo`. A binary search: the key
/// attributes' encodings are self-delimiting, so no item past `lo` is followed by one
/// that isn't.
fn first_above(items: &Leaf, lo: &[u8], nkey: usize) -> usize {
    let i = items.partition_point_keys(nkey, |k, _| !above(k, lo));
    debug_assert!((i..items.len()).all(|j| above(items.key_bytes(j, nkey), lo)), "items out of order");
    i
}

#[derive(Clone, Debug, Default)]
struct BPage {
    leaf: bool,
    items: Leaf,
    inner: Vec<(Pivot, u32)>,
    high: Option<Pivot>,
    right: Option<u32>,
    /// Bytes of data items (MAXALIGNed, line pointers excluded).
    used: usize,
}

impl BPage {
    fn nitems(&self) -> usize {
        if self.leaf {
            self.items.len()
        } else {
            self.inner.len()
        }
    }

    /// PageGetFreeSpace.
    fn free(&self) -> usize {
        let hk = self.high.as_ref().map_or(0, |h| h.size + ITEM_ID);
        PAGE_SPACE
            .saturating_sub(self.used + hk + ITEM_ID * self.nitems())
            .saturating_sub(ITEM_ID)
    }
}

/// How an index's entries behave.
#[derive(Clone, Debug)]
pub struct BtreeOpts {
    /// Key attributes (INCLUDE columns follow them).
    pub nkey: usize,
    pub unique: bool,
    pub nulls_not_distinct: bool,
    /// Deduplication applies (every key opclass is "equalimage", no INCLUDE columns,
    /// deduplicate_items on).
    pub dedup: bool,
    pub fillfactor: usize,
}

/// What an insertion knows about the rows behind the page's entries.
pub struct InsertHint<'a> {
    /// A non-HOT UPDATE left this index's key unchanged.
    pub unchanged: bool,
    /// Whether a heap version is gone for everyone (bottom-up deletion may drop it).
    pub dead: &'a dyn Fn(Tid) -> bool,
}

/// A leaf page as `Btree::leaf_view` shows it, borrowed.
pub struct LeafView<'a> {
    /// The high key's kept key attributes (`key_bytes`); None on the rightmost page.
    pub high: Option<&'a [u8]>,
    /// The high key has fewer than the index's key attributes (suffix truncation).
    pub truncated: bool,
    pub right: Option<u32>,
    entries: &'a Leaf,
    nkey: usize,
}

impl<'a> LeafView<'a> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry `i`'s key attributes, as an index entry's storage key has them.
    pub fn key(&self, i: usize) -> &'a [u8] {
        self.entries.get(i).key.key_bytes(self.nkey)
    }
}

/// What a btvacuumscan did (its IndexBulkDeleteResult): the heap TIDs it removed, the
/// index's pages, those it deleted, those deleted now, those it recorded as free to reuse.
#[derive(Clone, Copy, Debug, Default)]
pub struct BtVacuumed {
    pub removed: usize,
    pub pages: u32,
    pub newly_deleted: u32,
    pub deleted: u32,
    pub free: u32,
}

/// What `Btree::page_stats` counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct BtPageStats {
    pub root: u32,
    pub internal: u64,
    pub leaf: u64,
    /// Half-dead pages (none in the model).
    pub empty: u64,
    pub leaf_free: u64,
    pub fragments: u64,
    /// Pages VACUUM deleted (`delete_empty_pages`).
    pub deleted: u64,
    /// pgstattuple's: the leaves' live and LP_DEAD items and their tuples' bytes, and the
    /// free bytes of the leaves and deleted pages.
    pub items: u64,
    pub item_bytes: u64,
    pub dead_items: u64,
    pub dead_bytes: u64,
    pub free: u64,
}

/// The usable bytes of a btree page (pgstatindex's max_avail per leaf page).
pub const BT_PAGE_SPACE: usize = PAGE_SPACE;

#[derive(Clone, Debug)]
pub struct Btree {
    pub opts: BtreeOpts,
    /// By block number; block 0 is the metapage (None).
    pages: Vec<Option<BPage>>,
    root: Option<u32>,
    /// Heap TIDs indexed.
    pub tuples: usize,
    /// Bumped when its pages or height change.
    pub sizes: super::heapsim::SizeGen,
    /// The leaf the last insertion went to: the next one checks it first (keys added in
    /// order, a run of equal keys), skipping the descent when the leaf's own bounds hold it.
    last_leaf: u32,
    /// Where a scan from the start finds its first entry not LP_DEAD (`DeadHead`).
    head: DeadHead,
    /// Deleted pages (`delete_empty_pages`) by block, with the next xid when they were
    /// deleted (their safexid: recyclable once every transaction is past it). A deleted
    /// page without one (kept across a restart) is recyclable.
    safexids: std::collections::HashMap<u32, u64>,
    /// The index's free space map, once a VACUUM found a deleted page recyclable: splits
    /// take their new pages from it before extending the file (_bt_allocbuf).
    fsm: Option<Box<super::heapsim::Fsm>>,
    /// Deleted pages not in the free space map at the last VACUUM's scan of the index
    /// (the metapage's btm_last_cleanup_num_delpages).
    pending_delpages: u32,
}

/// A queue's index (rows inserted at one end, deleted at the other: `DELETE ... WHERE id =
/// (SELECT min(id) ...)`) has a run of leaves holding only LP_DEAD entries at its start until
/// VACUUM; a scan from the start marking what it passes (`kill`) walked every one of them.
/// This remembers (leftmost leaf, a leaf L) such that every leaf before L along the chain
/// holds no entry that isn't LP_DEAD (packed: leftmost << 32 | L; 0 unknown). Entries only
/// become LP_DEAD while it holds, except through an insertion into a leaf with none live
/// (`insert` forgets it then) and VACUUM's rewritten posting lists (`vacuum_scan` forgets
/// it). An atomic: walks under the models' read lock (`kill_needed`) move it on too.
#[derive(Debug, Default)]
struct DeadHead(std::sync::atomic::AtomicU64);

impl Clone for DeadHead {
    fn clone(&self) -> DeadHead {
        DeadHead(std::sync::atomic::AtomicU64::new(self.0.load(std::sync::atomic::Ordering::Relaxed)))
    }
}

impl DeadHead {
    fn get(&self, leftmost: u32) -> Option<u32> {
        let v = self.0.load(std::sync::atomic::Ordering::Relaxed);
        (v != 0 && (v >> 32) as u32 == leftmost).then_some(v as u32)
    }

    fn set(&self, leftmost: u32, at: u32) {
        self.0.store((leftmost as u64) << 32 | at as u64, std::sync::atomic::Ordering::Relaxed);
    }

    fn forget(&self) {
        self.0.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What `kill_walk` does at an entry not LP_DEAD that it passes.
enum Pass {
    Go,
    Mark,
}

impl Btree {
    pub fn new(opts: BtreeOpts) -> Btree {
        Btree {
            opts,
            pages: vec![None],
            root: None,
            tuples: 0,
            sizes: Default::default(),
            last_leaf: 0,
            head: DeadHead::default(),
            safexids: Default::default(),
            fsm: None,
            pending_delpages: 0,
        }
    }

    /// The free space map's pages.
    pub fn fsm_blocks(&self) -> u32 {
        self.fsm.as_ref().map_or(0, |f| f.blocks())
    }

    /// _bt_vacuum_needs_cleanup: a VACUUM that removed no entries still scans the index
    /// when the pages deleted but not yet free to reuse are more than 5% of it.
    pub fn needs_cleanup(&self) -> bool {
        self.pending_delpages > 0 && self.pending_delpages > self.nblocks() / 20
    }

    /// About the bytes the model holds (pages, items and their keys, pivots), for memory
    /// accounting.
    pub fn bytes(&self) -> usize {
        let mut n = self.pages.capacity() * size_of::<Option<BPage>>();
        for p in self.pages.iter().flatten() {
            n += p.items.bytes() + p.inner.capacity() * size_of::<(Pivot, u32)>();
            n += p.inner.iter().filter_map(|(pv, _)| pv.key.as_ref()).map(|k| k.as_bytes().len()).sum::<usize>();
        }
        n
    }

    pub fn nblocks(&self) -> u32 {
        self.pages.len() as u32
    }

    /// pgstatindex's walk over the pages after the metapage: (root block, internal pages,
    /// leaf pages, half-dead pages, leaf free bytes (PageGetExactFreeSpace), leaf
    /// fragments: leaves whose right sibling comes before them) and pgstattuple's over the
    /// leaves and deleted pages: (items, their bytes, dead items, their bytes, free bytes).
    pub fn page_stats(&self) -> BtPageStats {
        let mut s = BtPageStats { root: self.root.unwrap_or(0), ..Default::default() };
        for (b, p) in self.pages.iter().enumerate().skip(1) {
            let Some(p) = p else {
                // (A deleted page: pgstattuple counts it free, P_IGNORE.)
                s.deleted += 1;
                s.free += BLCKSZ as u64;
                continue;
            };
            // (pgstattuple's pgstat_btree_page reads leaf pages only: their exact free space
            // and data items, the high key not among them, LP_DEAD ones counted dead, each by
            // its tuple's length.)
            let free = p.free() + ITEM_ID;
            if p.leaf {
                s.free += free as u64;
                for it in p.items.iter() {
                    if it.dead {
                        s.dead_items += 1;
                        s.dead_bytes += it.size() as u64;
                    } else {
                        s.items += 1;
                        s.item_bytes += it.size() as u64;
                    }
                }
            }
            // (A leaf without items is a leaf page to pgstatindex too: its empty_pages are the
            // half-dead pages, P_IGNORE and not deleted, which a VACUUM of the model never
            // leaves, deleting a page in one step.)
            if !p.leaf {
                s.internal += 1;
            } else {
                s.leaf += 1;
                s.leaf_free += free as u64;
                if p.right.is_some_and(|r| (r as usize) < b) {
                    s.fragments += 1;
                }
            }
        }
        s
    }

    /// The root's level (0: the root is a leaf, or there is none).
    pub fn height(&self) -> u32 {
        let mut h = 0;
        let mut cur = self.root;
        while let Some(b) = cur {
            match self.pages.get(b as usize).and_then(|p| p.as_ref()) {
                Some(p) if !p.leaf => {
                    h += 1;
                    cur = p.inner.first().map(|x| x.1);
                }
                _ => break,
            }
        }
        h
    }

    fn page(&mut self, b: u32) -> &mut BPage {
        self.pages[b as usize].as_mut().expect("btree page")
    }

    fn alloc(&mut self, p: BPage) -> u32 {
        // (A deleted page the free space map has, else a new one: _bt_allocbuf.)
        if let Some(b) = self.fsm.as_mut().and_then(|f| f.get_free_index_page())
            && self.pages.get(b as usize).is_some_and(Option::is_none)
            && b != 0
        {
            self.pages[b as usize] = Some(p);
            self.safexids.remove(&b);
            self.sizes.bump();
            return b;
        }
        self.pages.push(Some(p));
        self.sizes.bump();
        self.pages.len() as u32 - 1
    }

    /// Where a new tuple sorts relative to a pivot (truncated attributes and a missing
    /// heap TID are minus infinity).
    fn cmp_pivot(&self, key: &Key, tid: Tid, p: &Pivot) -> Ordering {
        let Some(pk) = &p.key else {
            return Ordering::Greater;
        };
        let n = p.natts.min(self.opts.nkey);
        match key.cmp_keys(pk, n) {
            Ordering::Equal => {}
            x => return x,
        }
        if n < self.opts.nkey {
            return Ordering::Greater;
        }
        match p.tid {
            Some(t) => tid.cmp(&t),
            None => Ordering::Greater,
        }
    }

    /// A sorted build (CREATE INDEX, REINDEX, a rewrite) over entries in index order.
    pub fn build(opts: BtreeOpts, mut entries: Vec<(Key, Tid)>) -> Btree {
        let nkey = opts.nkey;
        entries.sort_by(|a, b| {
            a.0.cmp_keys(&b.0, nkey).then_with(|| a.1.cmp(&b.1))
        });
        let mut b = Builder {
            tree: Btree::new(opts.clone()),
            levels: Vec::new(),
        };
        b.tree.tuples = entries.len();
        let leaf_full = BLCKSZ * (100 - opts.fillfactor) / 100;
        if opts.dedup && !opts.unique {
            const MAX_POSTING: usize = (BLCKSZ * 10 / 100) / 8 * 8 - ITEM_ID;
            let mut pending: Option<(Key, Vec<Tid>)> = None;
            for (k, t) in entries {
                if let Some((base, tids)) = &mut pending
                    && base.keep_natts(&k, nkey) > nkey
                    && maxalign(base.size() + (tids.len() + 1) * 6) <= MAX_POSTING
                {
                    tids.push(t);
                    continue;
                }
                if let Some((base, tids)) = pending.take() {
                    let it = Item::posting(base, tids);
                    let extra = it.posting_bytes();
                    b.add_leaf(it, extra, leaf_full);
                }
                pending = Some((k, vec![t]));
            }
            if let Some((base, tids)) = pending {
                let it = Item::posting(base, tids);
                let extra = it.posting_bytes();
                b.add_leaf(it, extra, leaf_full);
            }
        } else {
            for (k, t) in entries {
                b.add_leaf(Item::plain(k, t), 0, leaf_full);
            }
        }
        b.finish()
    }

    /// Insert one entry (_bt_doinsert).
    pub fn insert(&mut self, key: Key, tid: Tid, hint: &InsertHint) {
        self.tuples += 1;
        let Some(root) = self.root else {
            let b = self.alloc(BPage {
                leaf: true,
                ..Default::default()
            });
            self.root = Some(b);
            let p = self.page(b);
            let it = Item::plain(key, tid);
            p.used += it.size();
            p.items.push(&it);
            return;
        };
        // The leaf the last insertion went to, if it holds this entry; else descend.
        let (mut blk, mut stack) = match self.holding_leaf(&key, tid) {
            Some(b) => (b, None),
            None => {
                let (b, s) = self.descend_for(root, &key, tid);
                (b, Some(s))
            }
        };
        self.last_leaf = blk;
        if self.pages[blk as usize].as_ref().is_some_and(|p| p.items.live() == 0) {
            self.head.forget();
        }
        let nkey = self.opts.nkey;
        // (_bt_doinsert bypasses uniqueness for NULLs only when they are distinct.)
        let checking = self.opts.unique
            && (self.opts.nulls_not_distinct || !key.has_null(self.opts.nkey));
        if checking {
            self.kill_dead_duplicates(blk, &key, hint.dead);
        }
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        let pos = Self::insert_pos(&p.items, &key, tid, nkey);
        let new = Item::plain(key, tid);
        let itemsz = new.size();
        let uniquedup = checking
            && [pos.checked_sub(1), Some(pos)]
                .into_iter()
                .flatten()
                .filter(|&i| i < p.items.len())
                .any(|i| p.items.get(i).key.keep_natts(&new.key, nkey) > nkey);
        let mut pos = pos;
        if p.free() < itemsz {
            self.delete_or_dedup(blk, &new.key, itemsz, checking, uniquedup, hint);
            let p = self.pages[blk as usize].as_ref().expect("btree page");
            pos = Self::insert_pos(&p.items, &new.key, tid, nkey);
        }
        let (new, pos) = self.posting_split(blk, new, tid, pos, hint);
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        if p.free() >= itemsz {
            let p = self.page(blk);
            p.used += itemsz;
            p.items.insert(pos, &new);
            return;
        }
        // (A split needs the path down: the descent's, also for a leaf the cache gave.)
        let stack = match stack.take() {
            Some(s) => s,
            None => {
                let (b, s) = self.descend_for(root, &new.key, tid);
                debug_assert_eq!(b, blk, "the cached leaf is the descent's");
                blk = b;
                s
            }
        };
        self.split_leaf(blk, pos, new, stack);
    }

    /// The descent to the leaf (`key`, `tid`) goes on, with the path: (page, downlink) per
    /// level.
    fn descend_for(&self, root: u32, key: &Key, tid: Tid) -> (u32, Vec<(u32, usize)>) {
        let mut stack: Vec<(u32, usize)> = Vec::new();
        let mut blk = root;
        loop {
            let p = self.pages[blk as usize].as_ref().expect("btree page");
            if p.leaf {
                return (blk, stack);
            }
            let i = p
                .inner
                .partition_point(|(pv, _)| self.cmp_pivot(key, tid, pv) == Ordering::Greater)
                .saturating_sub(1);
            stack.push((blk, i));
            blk = p.inner[i].1;
        }
    }

    /// The last insertion's leaf when (`key`, `tid`) belongs on it: past its first entry and
    /// before its high key (the rightmost leaf has none), so a descent would end there.
    fn holding_leaf(&self, key: &Key, tid: Tid) -> Option<u32> {
        let p = self.pages.get(self.last_leaf as usize)?.as_ref()?;
        if !p.leaf || p.items.is_empty() {
            return None;
        }
        let first = p.items.get(0);
        let after_first = match first.key.key_bytes(self.opts.nkey).cmp(key.key_bytes(self.opts.nkey)) {
            Ordering::Less => true,
            Ordering::Equal => first.tids.first() < tid,
            Ordering::Greater => false,
        };
        let below_high = p.high.as_ref().is_none_or(|h| self.cmp_pivot(key, tid, h) == Ordering::Less);
        (after_first && below_high).then_some(self.last_leaf)
    }

    /// _bt_binsrch_posting and _bt_swap_posting: a heap TID inside the TID range of an equal
    /// key's posting list (the item before `pos`, which orders items by their first TIDs) goes
    /// into it, and the list's last TID goes in as the new item, right after it: the list keeps
    /// its size, and the page gets one item more. A posting list marked LP_DEAD goes first
    /// (simple deletion: a split would clear its mark), and the new item goes where it falls.
    fn posting_split(&mut self, blk: u32, new: Item, tid: Tid, pos: usize, hint: &InsertHint) -> (Item, usize) {
        let nkey = self.opts.nkey;
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        let Some(at) = pos.checked_sub(1) else { return (new, pos) };
        let it = p.items.get(at);
        if it.tids.len() < 2 || !(it.tids.first() < tid && tid < it.tids.last()) || it.key.cmp_keys(&new.key, nkey) != Ordering::Equal {
            return (new, pos);
        }
        if it.dead {
            self.simple_delete(blk, hint);
            let p = self.pages[blk as usize].as_ref().expect("btree page");
            let pos = Self::insert_pos(&p.items, &new.key, tid, nkey);
            return (new, pos);
        }
        let mut items = self.page(blk).items.take();
        let mut tids = items[at].tids.to_vec();
        let last = tids.pop().expect("a posting list");
        let i = tids.partition_point(|&t| t < tid);
        tids.insert(i, tid);
        items[at] = Item::posting(items[at].key.clone(), tids);
        self.page(blk).items.put(&items);
        (Item::plain(new.key, last), pos)
    }

    /// Where (`key`, `tid`) goes among a leaf's items: by the key attributes' encodings
    /// (self-delimiting: they compare as the attributes do), then heap TID.
    fn insert_pos(items: &Leaf, key: &Key, tid: Tid, nkey: usize) -> usize {
        let k = key.key_bytes(nkey);
        items.partition_point_keys(nkey, |ik, i| match ik.cmp(k) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => items.tids(i).first() < tid,
        })
    }

    /// _bt_check_unique's hints: the entries with the new entry's key whose heap tuples are
    /// all dead to everyone get LP_DEAD (the next simple deletion pass removes them), from
    /// leaf `blk` on to the right while the key's entries go on.
    fn kill_dead_duplicates(&mut self, mut blk: u32, key: &Key, dead: &dyn Fn(Tid) -> bool) {
        let nkey = self.opts.nkey;
        loop {
            let p = self.page(blk);
            let mut i = p.items.partition_point(|it| it.key.cmp_keys(key, nkey) == Ordering::Less);
            while i < p.items.len() {
                let it = p.items.get(i);
                if it.key.cmp_keys(key, nkey) != Ordering::Equal {
                    return;
                }
                if !it.dead && it.tids.iter().all(dead) {
                    p.items.set_dead(i);
                }
                i += 1;
            }
            // (Every entry here has the key: the right sibling's may too, if its high key
            // keeps every key attribute and has it.)
            match (&p.high, p.right) {
                (Some(Pivot { key: Some(h), natts, .. }), Some(r)) if *natts >= nkey && h.cmp_keys(key, nkey) == Ordering::Equal => blk = r,
                _ => return,
            }
        }
    }

    /// _bt_delete_or_dedup_one_page: bottom-up deletion when an UPDATE's duplicates pile
    /// up, then a deduplication pass.
    fn delete_or_dedup(
        &mut self,
        blk: u32,
        key: &Key,
        itemsz: usize,
        checking: bool,
        uniquedup: bool,
        hint: &InsertHint,
    ) {
        // Simple deletion of the LP_DEAD items first (_bt_simpledel_pass), and of the
        // entries pointing into the same heap blocks that are dead too.
        let mut uniquedup = uniquedup;
        if self.simple_delete(blk, hint) {
            if self.pages[blk as usize].as_ref().expect("btree page").free() >= itemsz {
                return;
            }
            uniquedup = true;
        }
        if checking && !uniquedup {
            return;
        }
        let bottomup = hint.unchanged || uniquedup;
        if bottomup && self.bottomup_pass(blk, itemsz + ITEM_ID, hint.dead) {
            return;
        }
        if self.opts.dedup {
            self.dedup_pass(blk, key, itemsz, bottomup);
        }
    }

    /// _bt_simpledel_pass: the LP_DEAD items go, and the entries pointing into the same heap
    /// blocks that are dead too. False when the leaf has no LP_DEAD item.
    fn simple_delete(&mut self, blk: u32, hint: &InsertHint) -> bool {
        let p = self.page(blk);
        if p.items.iter().any(|it| it.dead) {
            let blocks: std::collections::HashSet<u32> = p
                .items
                .iter()
                .filter(|it| it.dead)
                .flat_map(|it| it.tids.iter().map(|t| t.0))
                .collect();
            let mut kept = Vec::with_capacity(p.items.len());
            for mut it in p.items.take() {
                if it.dead {
                    continue;
                }
                let before = it.tids.len();
                it.tids
                    .retain(|t| !(blocks.contains(&t.0) && (hint.dead)(*t)));
                if it.tids.is_empty() {
                    continue;
                }
                if it.tids.len() != before {
                    it = Item::posting(it.key, it.tids);
                }
                kept.push(it);
            }
            p.items.put(&kept);
            p.used = kept.iter().map(|i| i.size()).sum();
            return true;
        }
        false
    }

    /// _bt_bottomupdel_pass, with heap_index_delete_tuples' bottom-up choice of heap blocks
    /// (bottomup_sort_and_shrink) and _bt_delitems_delete_check: the page's TIDs grouped by
    /// heap block, the blocks with the most promising TIDs (duplicates') visited first, at
    /// most six, until the space freed reaches the target. True when the page needs no
    /// deduplication pass after it (no duplicates, or room enough now).
    fn bottomup_pass(&mut self, blk: u32, newitemsz: usize, dead: &dyn Fn(Tid) -> bool) -> bool {
        const MAX_NBLOCKS: usize = 6;
        const TOLERANCE_NBLOCKS: i64 = 3;
        let nkey = self.opts.nkey;
        let p = self.page(blk);
        if p.items.is_empty() {
            return true;
        }
        // (tid, item, promising, space freed)
        let mut deltids: Vec<(Tid, usize, bool, usize)> = Vec::new();
        let mut nintervals = 0;
        let mut i = 0;
        while i < p.items.len() {
            let base = p.items.get(i);
            let mut nhtids = base.tids.len();
            let mut j = i + 1;
            while j < p.items.len() {
                let it = p.items.get(j);
                // (_bt_dedup_save_htid against a maxpostingsize of BLCKSZ.)
                if base.key.keep_natts(it.key, nkey) <= nkey
                    || maxalign(base.key.size() + (nhtids + it.tids.len()) * 6) > BLCKSZ
                {
                    break;
                }
                nhtids += it.tids.len();
                j += 1;
            }
            let dup = j - i > 1;
            nintervals += dup as usize;
            for k in i..j {
                let it = p.items.get(k);
                let n = it.tids.len();
                if n == 1 {
                    deltids.push((it.tids.first(), k, dup, it.size() + ITEM_ID));
                    continue;
                }
                let (lo, mid, hi) = (it.tids.first().0, it.tids.get(n / 2).0, it.tids.last().0);
                let first = dup && lo == mid;
                let last = dup && !first && mid == hi;
                for (q, t) in it.tids.iter().enumerate() {
                    deltids.push((t, k, (first && q == 0) || (last && q == n - 1), 6));
                }
            }
            i = j;
        }
        // index_delete_sort, then the heap blocks' groups in bottomup_sort_and_shrink's order.
        deltids.sort_by_key(|d| d.0);
        // (first deltid, ntids, npromising)
        let mut groups: Vec<(usize, usize, usize)> = Vec::new();
        for (x, d) in deltids.iter().enumerate() {
            match groups.last_mut() {
                Some(g) if deltids[g.0].0.0 == d.0.0 => {
                    g.1 += 1;
                    g.2 += d.2 as usize;
                }
                _ => groups.push((x, 1, d.2 as usize)),
            }
        }
        for g in &mut groups {
            g.2 = if g.2 <= 4 { 4 } else { g.2.next_power_of_two() };
        }
        groups.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| if a.1 == b.1 { Ordering::Equal } else { b.1.next_power_of_two().cmp(&a.1.next_power_of_two()) })
                .then(a.0.cmp(&b.0))
        });
        groups.truncate(MAX_NBLOCKS);
        let mut favorable = 0;
        let mut lastblock: i64 = -1;
        for g in &groups {
            let b = deltids[g.0].0.0 as i64;
            if lastblock != -1 && (b < lastblock - TOLERANCE_NBLOCKS || b > lastblock + TOLERANCE_NBLOCKS) {
                break;
            }
            favorable += 1;
            lastblock = b;
        }
        // heap_index_delete_tuples' visit.
        let mut target = (BLCKSZ / 16).max(newitemsz);
        let (mut actual, mut last_free, mut accessed, mut final_block) = (0, 0, 0, false);
        let mut gone: std::collections::HashSet<(usize, Tid)> = Default::default();
        'blocks: for g in &groups {
            if final_block || (accessed >= 1 && actual == last_free) {
                break;
            }
            last_free = actual;
            if favorable > 0 {
                favorable -= 1;
            } else {
                target /= 2;
            }
            accessed += 1;
            for d in &deltids[g.0..g.0 + g.1] {
                if !dead(d.0) {
                    continue;
                }
                gone.insert((d.1, d.0));
                actual += d.3;
                if actual >= target {
                    final_block = true;
                }
            }
            if final_block {
                break 'blocks;
            }
        }
        if !gone.is_empty() {
            let p = self.page(blk);
            let mut kept = Vec::with_capacity(p.items.len());
            for (k, mut it) in p.items.take().into_iter().enumerate() {
                let before = it.tids.len();
                it.tids.retain(|t| !gone.contains(&(k, *t)));
                if it.tids.is_empty() {
                    continue;
                }
                if it.tids.len() != before {
                    it = Item::posting(it.key, it.tids);
                }
                kept.push(it);
            }
            p.items.put(&kept);
            p.used = kept.iter().map(|i| i.size()).sum();
        }
        if nintervals == 0 {
            return true;
        }
        let p = self.page(blk);
        p.free() + ITEM_ID >= (BLCKSZ / 24).max(newitemsz)
    }

    /// _bt_dedup_pass.
    fn dedup_pass(&mut self, blk: u32, newkey: &Key, newitemsz: usize, bottomup: bool) {
        let nkey = self.opts.nkey;
        let newitemsz = newitemsz + ITEM_ID;
        let p = self.page(blk);
        if p.items.is_empty() {
            return;
        }
        let mut maxposting = MAX_ITEM / 2;
        let mut singleval = !bottomup
            && newkey.keep_natts(p.items.get(0).key, nkey) > nkey
            && newkey.keep_natts(p.items.get(p.items.len() - 1).key, nkey) > nkey;
        let mut deduplicate = true;
        let mut nmaxitems = 0;
        let mut nintervals = 0;
        let mut out: Vec<Item> = Vec::with_capacity(p.items.len());
        let mut pending: Option<(Key, Vec<Tid>, usize)> = None;
        let finish = |pending: Option<(Key, Vec<Tid>, usize)>,
                      out: &mut Vec<Item>,
                      nintervals: &mut usize| {
            if let Some((k, tids, nitems)) = pending {
                if nitems > 1 {
                    *nintervals += 1;
                }
                out.push(Item::posting(k, tids));
            }
        };
        for it in p.items.take() {
            if let Some((base, tids, nitems)) = &mut pending
                && deduplicate
                && base.keep_natts(&it.key, nkey) > nkey
            {
                if maxalign(base.size() + (tids.len() + it.tids.len()) * 6) <= maxposting {
                    tids.extend_from_slice(&it.tids);
                    *nitems += 1;
                    continue;
                }
                if tids.len() > 50 {
                    nmaxitems += 1;
                }
            }
            finish(pending.take(), &mut out, &mut nintervals);
            if singleval {
                if nmaxitems == 5 {
                    let leftfree = PAGE_SPACE - (newitemsz + PIVOT_TID);
                    let reduction =
                        (leftfree as f64 * ((100 - SINGLEVAL_FILLFACTOR) as f64 / 100.0)) as usize;
                    maxposting = maxposting.saturating_sub(reduction);
                } else if nmaxitems == 6 {
                    deduplicate = false;
                    singleval = false;
                }
            }
            pending = Some((it.key, it.tids.into_vec(), 1));
        }
        finish(pending.take(), &mut out, &mut nintervals);
        let p = self.page(blk);
        p.used = out.iter().map(|i| i.size()).sum();
        p.items.put(&out);
    }

    /// _bt_split on a leaf, then the downlink into the parent.
    fn split_leaf(&mut self, blk: u32, pos: usize, new: Item, stack: Vec<(u32, usize)>) {
        let nkey = self.opts.nkey;
        let fillfactor = self.opts.fillfactor;
        // (The page's items, unpacked while the split is chosen and made.)
        let mut all: Vec<Item> = self.page(blk).items.take();
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        let rightmost = p.right.is_none();
        let (firstright, newonleft) = {
            let items = &all;
            let at = |v: V| -> &Item {
                match v {
                    V::New => &new,
                    V::Orig(i) => &items[i],
                }
            };
            let keep = |a: V, b: V| at(a).key.keep_natts(&at(b).key, nkey);
            let afternew = |s: &SplitState| -> Option<bool> {
                // _bt_afternewitemoff.
                if nkey == 1 || s.newitemoff == 0 {
                    return None;
                }
                let n = items.len();
                if s.newitemsz != s.minfirstrightsz {
                    return None;
                }
                if s.newitemsz * n != s.olddataitemstotal {
                    return None;
                }
                if s.newitemsz > maxalign(8 + 16) + ITEM_ID {
                    return None;
                }
                if s.newitemoff >= n {
                    let k = keep(V::Orig(n - 1), V::New);
                    return (k > 1 && k <= nkey).then_some(true);
                }
                let prev = &items[s.newitemoff - 1];
                let (lo, hi) = (prev.tids[0], new.tids[0]);
                let adjacent = lo.0 == hi.0 || (lo.0 + 1 == hi.0 && hi.1 == 1);
                if prev.tids.len() > 1 || !adjacent {
                    return None;
                }
                let k = keep(V::Orig(s.newitemoff - 1), V::New);
                if k > 1 && k <= nkey {
                    let interp = (s.newitemoff + 2) as f64 / (n as f64 + 2.0);
                    Some(interp > fillfactor as f64 / 100.0)
                } else {
                    None
                }
            };
            let input = SplitInput {
                is_leaf: true,
                is_rightmost: rightmost,
                hk: p.high.as_ref().map(|h| h.size),
                sizes: items.iter().map(|i| i.size()).collect(),
                posting: items.iter().map(|i| i.posting_bytes()).collect(),
                newitemoff: pos,
                newitemsz: new.size(),
                fillfactor,
                nkey,
            };
            find_split(&input, &keep, &afternew, || {
                // High key vs new item for the single value strategy.
                p.high
                    .as_ref()
                    .and_then(|h| h.key.as_ref().map(|k| (k.clone(), h.natts)))
                    .map_or(1, |(k, natts)| {
                        if natts < nkey {
                            k.keep_natts(&new.key, natts).min(natts)
                        } else {
                            k.keep_natts(&new.key, nkey)
                        }
                    })
            })
        };
        all.insert(pos, new);
        // Position of firstright in the combined list.
        let cut = if newonleft || firstright > pos {
            firstright + 1
        } else {
            firstright
        };
        let cut = if !newonleft && firstright == pos {
            pos
        } else {
            cut
        };
        // (Items too big for the page's model can leave a half empty: keep one each side.)
        let cut = cut.clamp(1, all.len().saturating_sub(1).max(1));
        let right_items = all.split_off(cut);
        let lastleft = all.last().expect("left half").clone();
        let firstright_item = right_items.first().expect("right half").clone();
        let keepn = lastleft.key.keep_natts(&firstright_item.key, nkey);
        let high = if keepn <= nkey {
            Pivot {
                key: Some(firstright_item.key.clone()),
                natts: keepn,
                tid: None,
                size: firstright_item.key.prefix_size(keepn),
            }
        } else {
            Pivot {
                key: Some(firstright_item.key.clone()),
                natts: nkey,
                tid: lastleft.tids.last().copied(),
                size: firstright_item.key.prefix_size(nkey) + PIVOT_TID,
            }
        };
        let old = self.page(blk);
        let old_high = old.high.take();
        let old_right = old.right;
        let mut right = BPage {
            leaf: true,
            used: right_items.iter().map(|i| i.size()).sum(),
            items: Leaf::default(),
            inner: Vec::new(),
            high: old_high,
            right: old_right,
        };
        right.items.put(&right_items);
        let rblk = self.alloc(right);
        let left = self.page(blk);
        left.used = all.iter().map(|i| i.size()).sum();
        left.items.put(&all);
        left.high = Some(high.clone());
        left.right = Some(rblk);
        self.insert_parent(blk, rblk, high, stack);
    }

    /// _bt_insert_parent: the downlink to a new right sibling.
    fn insert_parent(&mut self, left: u32, right: u32, pivot: Pivot, mut stack: Vec<(u32, usize)>) {
        let Some((pblk, idx)) = stack.pop() else {
            // The root split: a new root above.
            let root = BPage {
                leaf: false,
                items: Leaf::default(),
                used: MINUS_INF + pivot.size,
                inner: vec![(Pivot::minus_inf(), left), (pivot, right)],
                high: None,
                right: None,
            };
            let r = self.alloc(root);
            self.root = Some(r);
            return;
        };
        let itemsz = pivot.size;
        let p = self.page(pblk);
        let pos = idx + 1;
        if p.free() >= itemsz {
            p.used += itemsz;
            p.inner.insert(pos, (pivot, right));
            return;
        }
        // Split the internal page.
        let p = self.pages[pblk as usize].as_ref().expect("btree page");
        let rightmost = p.right.is_none();
        let (firstright, newonleft) = {
            let keep = |_: V, _: V| 1usize;
            let input = SplitInput {
                is_leaf: false,
                is_rightmost: rightmost,
                hk: p.high.as_ref().map(|h| h.size),
                sizes: p.inner.iter().map(|i| i.0.size).collect(),
                posting: vec![0; p.inner.len()],
                newitemoff: pos,
                newitemsz: itemsz,
                fillfactor: NONLEAF_FILLFACTOR,
                nkey: self.opts.nkey,
            };
            find_split(&input, &keep, &|_| None, || 1)
        };
        let mut all = std::mem::take(&mut self.page(pblk).inner);
        all.insert(pos, (pivot, right));
        let cut = if !newonleft && firstright == pos {
            pos
        } else if newonleft || firstright > pos {
            firstright + 1
        } else {
            firstright
        };
        let mut right_items = all.split_off(cut);
        let high = right_items[0].0.clone();
        right_items[0].0 = Pivot::minus_inf();
        let old = self.page(pblk);
        let old_high = old.high.take();
        let old_right = old.right;
        let rpage = BPage {
            leaf: false,
            items: Leaf::default(),
            used: right_items.iter().map(|i| i.0.size).sum(),
            inner: right_items,
            high: old_high,
            right: old_right,
        };
        let rblk = self.alloc(rpage);
        let lp = self.page(pblk);
        lp.used = all.iter().map(|i| i.0.size).sum();
        lp.inner = all;
        lp.high = Some(high.clone());
        lp.right = Some(rblk);
        self.insert_parent(pblk, rblk, high, stack);
    }

    /// The leaf page where storage key `lo` is (or would go).
    fn descend(&self, lo: Option<&[u8]>) -> Option<u32> {
        let nkey = self.opts.nkey;
        let mut blk = self.root?;
        loop {
            let p = self.pages.get(blk as usize)?.as_ref()?;
            if p.leaf {
                return Some(blk);
            }
            let i = match lo {
                None => 0,
                Some(lo) => child_for(&p.inner, lo, nkey),
            };
            blk = p.inner[i].1;
        }
    }

    /// The index tuples whose key attributes are `key` (their encoding), in index order:
    /// each one's heap TIDs, ascending (a posting list's). What orders a backward scan's
    /// equal keys: tuples last first, a posting list's TIDs ascending (_bt_readpage).
    pub fn postings(&self, key: &[u8]) -> Vec<Vec<Tid>> {
        let nkey = self.opts.nkey;
        let mut out = Vec::new();
        let Some(mut blk) = self.descend(Some(key)) else {
            return out;
        };
        loop {
            let Some(p) = self.pages.get(blk as usize).and_then(|p| p.as_ref()) else {
                return out;
            };
            for it in p.items.iter() {
                let k = it.key.key_bytes(nkey);
                if k > key {
                    return out;
                }
                if k == key {
                    out.push(it.tids.to_vec());
                }
            }
            match p.right {
                Some(r) => blk = r,
                None => return out,
            }
        }
    }

    /// The leaf page an entry with key `key` goes on (what an insertion's SSI check names).
    pub fn leaf_for(&self, key: &Key) -> Option<u32> {
        self.descend(Some(key.key_bytes(self.opts.nkey)))
    }

    /// The leaf pages an index scan over storage keys [lo, hi) reads (the pages its SIRead
    /// locks go on: _bt_first's, then each _bt_readnextpage's). Empty: the index has no
    /// pages yet.
    pub fn leaf_pages(&self, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Vec<u32> {
        let nkey = self.opts.nkey;
        let mut out = Vec::new();
        let Some(mut blk) = self.descend(lo) else {
            return out;
        };
        loop {
            let Some(p) = self.pages.get(blk as usize).and_then(|p| p.as_ref()) else {
                return out;
            };
            out.push(blk);
            if hi.is_some_and(|hi| p.items.last().is_some_and(|it| it.key.key_bytes(nkey) >= hi)) {
                return out;
            }
            match p.right {
                Some(r) => blk = r,
                None => return out,
            }
        }
    }

    /// An index scan over storage keys [lo, hi) (key attributes' encodings; None:
    /// unbounded) met the entries there: those whose heap tuples are all dead to everyone
    /// are marked LP_DEAD (kill_prior_tuple, _bt_killitems). `seen` gets the heap TIDs of
    /// the entries the scan returns (those not LP_DEAD already), in index order.
    pub fn kill(
        &mut self,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        all_dead: &dyn Fn(Tid) -> bool,
        mut seen: Option<&mut Vec<Tid>>,
    ) {
        let mut marks: Vec<(u32, usize)> = Vec::new();
        self.kill_walk(lo, hi, &mut |blk, i, tids| {
            if let Some(seen) = seen.as_deref_mut() {
                seen.extend(tids.iter());
            }
            if tids.iter().all(all_dead) {
                marks.push((blk, i));
            }
            Pass::Go
        });
        for (blk, i) in marks {
            if let Some(p) = self.pages.get_mut(blk as usize).and_then(|p| p.as_mut()) {
                p.items.set_dead(i);
            }
        }
    }

    /// The entries `kill` would mark that a scan of [lo, hi) passes before the one pointing
    /// at heap TID `live`, where it stops (RelationFindReplTupleByIndex returns that row):
    /// (leaf, item) each, for `set_dead`; only the first with `first`.
    pub fn dead_before(&self, lo: Option<&[u8]>, hi: Option<&[u8]>, all_dead: &dyn Fn(Tid) -> bool, live: Tid, first: bool) -> Vec<(u32, usize)> {
        let mut marks: Vec<(u32, usize)> = Vec::new();
        self.kill_walk(lo, hi, &mut |blk, i, tids| {
            if tids.iter().any(|t| t == live) {
                return Pass::Mark;
            }
            if tids.iter().all(all_dead) {
                marks.push((blk, i));
                if first {
                    return Pass::Mark;
                }
            }
            Pass::Go
        });
        marks
    }

    /// These entries (`dead_before`'s) marked LP_DEAD.
    pub fn set_dead(&mut self, marks: &[(u32, usize)]) {
        for &(blk, i) in marks {
            if let Some(p) = self.pages.get_mut(blk as usize).and_then(|p| p.as_mut()) {
                p.items.set_dead(i);
            }
        }
    }

    /// `kill`'s and `kill_needed`'s walk: down to the leaf where `lo` starts, then right
    /// along the leaves until past `hi`, calling `each(leaf, item, its TIDs)` for every
    /// entry in range not LP_DEAD already, in index order, until it says `Mark` (an entry
    /// to mark found: the walk stops). Leaves with no entry that isn't LP_DEAD are passed
    /// whole; a walk from the start begins past the run of them there (`DeadHead`).
    /// Returns whether `each` said `Mark`.
    fn kill_walk(
        &self,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        each: &mut dyn FnMut(u32, usize, leaf::Tids) -> Pass,
    ) -> bool {
        // (An empty bound is below every key: a scan of the whole index has one.)
        let lo = lo.filter(|l| !l.is_empty());
        let nkey = self.opts.nkey;
        let Some(mut blk) = self.root else {
            return false;
        };
        // Down to the leaf where `lo` starts (pivots compare as their kept attributes).
        loop {
            let Some(p) = self.pages.get(blk as usize).and_then(|p| p.as_ref()) else {
                return false;
            };
            if p.leaf {
                break;
            }
            let i = match lo {
                None => 0,
                Some(lo) => child_for(&p.inner, lo, nkey),
            };
            blk = p.inner[i].1;
        }
        // From the start: past the leaves known to hold only LP_DEAD entries, when that
        // walk wouldn't have stopped in them (their keys are at most the first one's of
        // the leaf it goes to: below `hi` when that is).
        let leftmost = lo.is_none().then_some(blk);
        if let Some(l) = leftmost
            && let Some(at) = self.head.get(l)
            && let Some(p) = self.pages.get(at as usize).and_then(|p| p.as_ref())
            && p.leaf
            && hi.is_none_or(|hi| !p.items.is_empty() && p.items.key_bytes(0, nkey) < hi)
        {
            blk = at;
        }
        // Then right along the leaves until past `hi`. (A storage key goes on with the
        // row's key: an entry is past `lo` when some row key puts it there.)
        // (Once a leaf has an item past `lo`, every item after it is.)
        let mut lo = lo;
        let mut dead_run = leftmost.is_some();
        loop {
            let Some(p) = self.pages.get(blk as usize).and_then(|p| p.as_ref()) else {
                return false;
            };
            if dead_run && (p.items.live() > 0 || p.right.is_none()) {
                dead_run = false;
                if let Some(l) = leftmost
                    && self.head.get(l) != Some(blk)
                {
                    self.head.set(l, blk);
                }
            }
            let from = lo.map_or(0, |l| first_above(&p.items, l, nkey));
            if from < p.items.len() {
                lo = None;
            }
            let last = p.items.len().saturating_sub(1);
            let whole = hi.is_none_or(|hi| p.items.is_empty() || p.items.key_bytes(last, nkey) < hi);
            if p.items.live() == 0 {
                // (Every entry LP_DEAD: the walk would only stop at the last, past `hi`.)
                if from < p.items.len() && !whole {
                    return false;
                }
            } else {
                for i in from..p.items.len() {
                    // (Decoded only as far as the walk needs: an entry's key only to stop at.)
                    let dead = p.items.is_dead(i);
                    if !whole
                        && (!dead || i == last)
                        && hi.is_some_and(|hi| p.items.key_bytes(i, nkey) >= hi)
                    {
                        return false;
                    }
                    if dead {
                        continue;
                    }
                    match each(blk, i, p.items.tids(i)) {
                        Pass::Go => {}
                        Pass::Mark => return true,
                    }
                }
            }
            match p.right {
                Some(r) => blk = r,
                None => return false,
            }
        }
    }

    /// The leaf page a scan starting at model key `lo` reads first: the one with its first
    /// entry past `lo` (a following page when the page the descent reaches has none), or
    /// the last leaf the descent reached. None: no leaf.
    /// Leaf page `blk` as a scan walks it (a skip scan's "Index Searches", agent 144): its
    /// entries' key attributes, its high key's and its right sibling. None: not a leaf.
    pub fn leaf_view(&self, blk: u32) -> Option<LeafView<'_>> {
        let p = self.pages.get(blk as usize)?.as_ref()?;
        if !p.leaf {
            return None;
        }
        let nkey = self.opts.nkey;
        let high = p.high.as_ref().and_then(|h| Some((h.key.as_ref()?.key_bytes(h.natts.min(nkey)), h.natts < nkey)));
        Some(LeafView { high: high.map(|h| h.0), truncated: high.is_some_and(|h| h.1), right: p.right, entries: &p.items, nkey })
    }

    pub fn scan_leaf(&self, lo: &[u8]) -> Option<u32> {
        let nkey = self.opts.nkey;
        let mut blk = self.root?;
        loop {
            let p = self.pages.get(blk as usize)?.as_ref()?;
            if p.leaf {
                break;
            }
            blk = p.inner[child_for(&p.inner, lo, nkey)].1;
        }
        loop {
            let p = self.pages.get(blk as usize)?.as_ref()?;
            if first_above(&p.items, lo, nkey) < p.items.len() {
                return Some(blk);
            }
            match p.right {
                Some(r) => blk = r,
                None => return Some(blk),
            }
        }
    }

    /// `kill`'s walk without its marks: whether it would mark an entry LP_DEAD (`seen`
    /// as `kill` fills it, up to that entry). Under the models' read lock, so concurrent
    /// scans of an index with nothing to kill don't serialize.
    pub fn kill_needed(
        &self,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        all_dead: &dyn Fn(Tid) -> bool,
        mut seen: Option<&mut Vec<Tid>>,
    ) -> bool {
        self.kill_walk(lo, hi, &mut |_, _, tids| {
            if let Some(seen) = seen.as_deref_mut() {
                seen.extend(tids.iter());
            }
            if tids.iter().all(all_dead) { Pass::Mark } else { Pass::Go }
        })
    }

    /// VACUUM's bulk delete: entries pointing at removed heap tuples go (pages stay).
    /// `tuples` becomes the heap TIDs left, as btvacuumscan counts them.
    pub fn bulk_delete(&mut self, dead: &crate::exec::fasthash::HashSet<Tid>) -> usize {
        self.vacuum_scan(Some(dead), &|_| false, 0).removed
    }

    /// btvacuumscan, from btbulkdelete (`dead`: the heap TIDs whose entries go, each page's
    /// packed in place, `bulk_delete`'s) or btvacuumcleanup (None, when `needs_cleanup`):
    /// the leaves left empty are deleted (`delete_empty_pages`, their safexid `next_xid`),
    /// and deleted pages whose safexid is `removable` are recorded in the free space map,
    /// those deleted earlier as the scan meets them, those it deleted at its end in the
    /// order it deleted them, until one isn't (_bt_pendingfsm_finalize).
    pub fn vacuum_scan(
        &mut self,
        dead: Option<&crate::exec::fasthash::HashSet<Tid>>,
        removable: &dyn Fn(u64) -> bool,
        next_xid: u64,
    ) -> BtVacuumed {
        // (Pages go: the hint names none of them.)
        self.head.forget();
        let mut free = 0u32;
        let recyclable = |safexids: &std::collections::HashMap<u32, u64>, b: u32| safexids.get(&b).is_none_or(|&x| removable(x));
        for b in 1..self.pages.len() as u32 {
            if self.pages[b as usize].is_none() && recyclable(&self.safexids, b) {
                self.fsm.get_or_insert_default().record_free_index_page(b);
                free += 1;
            }
        }
        let removed = match dead {
            Some(dead) => self.remove_tids(dead),
            None => 0,
        };
        // (Those deleted now share their safexid: they go to the map together, or none.)
        let now = removable(next_xid);
        let gone = self.delete_empty_pages();
        let newly_deleted = gone.len() as u32;
        for b in gone {
            self.safexids.insert(b, next_xid);
            if now {
                self.fsm.get_or_insert_default().record_free_index_page(b);
                free += 1;
            }
        }
        if free > 0
            && let Some(f) = self.fsm.as_mut()
        {
            f.vacuum_all();
        }
        let deleted = self.pages.iter().skip(1).filter(|p| p.is_none()).count() as u32;
        self.pending_delpages = deleted - free.min(deleted);
        BtVacuumed { removed, pages: self.nblocks(), newly_deleted, deleted, free }
    }

    /// The entries pointing at removed heap tuples go (pages stay). `tuples` becomes the
    /// heap TIDs left, as btvacuumscan counts them.
    fn remove_tids(&mut self, dead: &crate::exec::fasthash::HashSet<Tid>) -> usize {
        let mut removed = 0;
        let mut left = 0;
        for p in self.pages.iter_mut().flatten() {
            if !p.leaf {
                continue;
            }
            let n: usize = p.items.iter().map(|it| it.tids.len()).sum();
            if !p.items.iter().any(|it| it.tids.iter().any(|t| dead.contains(&t))) {
                left += n;
                continue;
            }
            let (r, l, used) = p.items.retain_tids(|t| !dead.contains(&t));
            removed += r;
            left += l;
            p.used = used;
        }
        self.tuples = left;
        removed
    }

    /// VACUUM's page deletion (btvacuumpage's attempt_pagedel, _bt_pagedel), in block
    /// order: a leaf left without items goes unless it is the rightmost on its level, or the
    /// rightmost of its parent's children without being the only one. An only child goes
    /// with its parent, and so on up to the first ancestor with other children
    /// (_bt_lock_subtree_parent: none of them may be the root or the rightmost on its
    /// level). The ancestor's downlink to the top of what goes now leads to that page's
    /// right sibling, whose own downlink goes (its key space reaches down to the deleted
    /// page's), and each deleted page's left sibling links past it. Their blocks stay in the
    /// file, deleted (None, as the metapage: `page_stats` counts them), and aren't reused.
    /// Returns the pages deleted, in the order they went.
    fn delete_empty_pages(&mut self) -> Vec<u32> {
        let n = self.pages.len();
        // Each page's parent with its downlink's place, and its left sibling.
        let mut parent: Vec<Option<(u32, usize)>> = vec![None; n];
        let mut left: Vec<Option<u32>> = vec![None; n];
        for (b, p) in self.pages.iter().enumerate() {
            let Some(p) = p else { continue };
            for (i, (_, c)) in p.inner.iter().enumerate() {
                parent[*c as usize] = Some((b as u32, i));
            }
            if let Some(r) = p.right {
                left[r as usize] = Some(b as u32);
            }
        }
        fn live(pages: &[Option<BPage>], b: u32) -> &BPage {
            pages[b as usize].as_ref().expect("btree page")
        }
        let mut deleted = Vec::new();
        for b in 1..n as u32 {
            if !self.pages[b as usize].as_ref().is_some_and(|p| p.leaf && p.items.is_empty() && p.right.is_some())
                || Some(b) == self.root
            {
                continue;
            }
            // Up to the parent that keeps a downlink: (it, the downlink's place).
            let mut chain = vec![b];
            let keeper = loop {
                let top = *chain.last().expect("chain");
                let Some((pb, i)) = parent[top as usize] else { break None };
                let pp = live(&self.pages, pb);
                if i + 1 < pp.inner.len() {
                    break Some((pb, i));
                }
                if pp.inner.len() > 1 || pp.right.is_none() || Some(pb) == self.root {
                    break None;
                }
                chain.push(pb);
            };
            let Some((kp, i)) = keeper else { continue };
            let top = *chain.last().expect("chain");
            let rs = live(&self.pages, top).right.expect("not rightmost");
            let k = self.page(kp);
            debug_assert_eq!(k.inner[i + 1].1, rs, "the right sibling's downlink follows");
            let (gone, _) = k.inner.remove(i + 1);
            k.used -= gone.size;
            k.inner[i].1 = rs;
            parent[rs as usize] = Some((kp, i));
            for (j, (_, c)) in self.page(kp).inner.iter().enumerate().skip(i + 1) {
                parent[*c as usize] = Some((kp, j));
            }
            for c in chain.iter().copied() {
                let r = live(&self.pages, c).right;
                if let Some(l) = left[c as usize] {
                    self.page(l).right = r;
                }
                if let Some(r) = r {
                    left[r as usize] = left[c as usize];
                }
                self.pages[c as usize] = None;
                parent[c as usize] = None;
                deleted.push(c);
            }
        }
        if !deleted.is_empty() {
            self.sizes.bump();
        }
        deleted
    }
}

/// Which tuple of the page-with-new-item a split refers to.
#[derive(Clone, Copy, Debug)]
enum V {
    Orig(usize),
    New,
}

struct SplitInput {
    is_leaf: bool,
    is_rightmost: bool,
    /// The page's high key size (not rightmost).
    hk: Option<usize>,
    /// Data items' MAXALIGNed sizes.
    sizes: Vec<usize>,
    /// Posting list bytes of each data item.
    posting: Vec<usize>,
    /// Where the new item goes among the data items.
    newitemoff: usize,
    newitemsz: usize,
    fillfactor: usize,
    nkey: usize,
}

#[derive(Clone, Copy, Debug)]
struct SplitPoint {
    curdelta: i16,
    leftfree: i16,
    rightfree: i16,
    /// The first original item on the right (newitemoff when the new item is it).
    firstright: usize,
    newonleft: bool,
}

struct SplitState {
    newitemoff: usize,
    newitemsz: usize,
    minfirstrightsz: usize,
    olddataitemstotal: usize,
}

#[derive(PartialEq, Eq)]
enum Strategy {
    Default,
    ManyDuplicates,
    SingleValue,
}

/// _bt_findsplitloc: (firstright, new item on left).
fn find_split(
    s: &SplitInput,
    keep: &dyn Fn(V, V) -> usize,
    afternew: &dyn Fn(&SplitState) -> Option<bool>,
    hikey_keep: impl Fn() -> usize,
) -> (usize, bool) {
    let leftspace = PAGE_SPACE as i32;
    let mut rightspace = PAGE_SPACE as i32;
    if let Some(h) = s.hk.filter(|_| !s.is_rightmost) {
        rightspace -= (maxalign(h) + ITEM_ID) as i32;
    }
    let total: usize = s.sizes.iter().map(|z| z + ITEM_ID).sum();
    let newitemsz = s.newitemsz + ITEM_ID;
    let mut st = SplitState {
        newitemoff: s.newitemoff,
        newitemsz,
        minfirstrightsz: usize::MAX,
        olddataitemstotal: total,
    };
    let mut splits: Vec<SplitPoint> = Vec::new();
    let mut rec = |st: &mut SplitState, firstright: usize, newonleft: bool, toleft: usize, frsz: usize| {
        let newisfirstright = firstright == s.newitemoff && !newonleft;
        let (firstrightsz, postingsz) = if newisfirstright {
            (newitemsz, 0)
        } else {
            let p = if s.is_leaf && frsz > 64 {
                s.posting[firstright]
            } else {
                0
            };
            (frsz, p)
        };
        let mut leftfree = leftspace - toleft as i32;
        let mut rightfree = rightspace - (total - toleft) as i32;
        if s.is_leaf {
            leftfree -= (firstrightsz + PIVOT_TID - postingsz) as i32;
        } else {
            leftfree -= firstrightsz as i32;
        }
        if newonleft {
            leftfree -= newitemsz as i32;
        } else {
            rightfree -= newitemsz as i32;
        }
        if !s.is_leaf {
            rightfree += firstrightsz as i32 - (8 + ITEM_ID) as i32;
        }
        let (leftfree, rightfree) = (leftfree as i16, rightfree as i16);
        if leftfree >= 0 && rightfree >= 0 {
            st.minfirstrightsz = st.minfirstrightsz.min(firstrightsz);
            splits.push(SplitPoint {
                curdelta: 0,
                leftfree,
                rightfree,
                firstright,
                newonleft,
            });
        }
    };
    let mut toleft = 0;
    for (i, sz) in s.sizes.iter().enumerate() {
        let itemsz = sz + ITEM_ID;
        match i.cmp(&s.newitemoff) {
            Ordering::Less => rec(&mut st, i, false, toleft, itemsz),
            Ordering::Greater => rec(&mut st, i, true, toleft, itemsz),
            Ordering::Equal => {
                rec(&mut st, i, false, toleft, itemsz);
                rec(&mut st, i, true, toleft, itemsz);
            }
        }
        toleft += itemsz;
    }
    if s.newitemoff >= s.sizes.len() {
        rec(&mut st, s.newitemoff, false, total, 0);
    }
    if splits.is_empty() {
        // Can't happen with sane sizes; split in the middle.
        let mid = s.sizes.len() / 2;
        return (mid, s.newitemoff < mid);
    }
    let (mut usemult, mut ffmult);
    if !s.is_leaf {
        usemult = s.is_rightmost;
        ffmult = NONLEAF_FILLFACTOR as f64 / 100.0;
    } else if s.is_rightmost {
        usemult = true;
        ffmult = s.fillfactor as f64 / 100.0;
    } else if let Some(um) = afternew(&st) {
        usemult = um;
        if um {
            ffmult = s.fillfactor as f64 / 100.0;
        } else {
            if let Some(sp) = splits
                .iter()
                .find(|sp| sp.newonleft && sp.firstright == s.newitemoff)
            {
                return (sp.firstright, true);
            }
            ffmult = 0.5;
        }
    } else {
        usemult = false;
        ffmult = 0.5;
    }
    let leftpage = splits[0];
    let rightpage = splits[splits.len() - 1];
    delta_sort(&mut splits, ffmult, usemult);
    let tolerance = if s.is_leaf {
        (total as f64 * 0.050) as i16
    } else {
        (total as f64 * 0.075) as i16
    };
    let mut interval = default_interval(&splits, tolerance);
    let lastleft = |sp: &SplitPoint| {
        if sp.newonleft && sp.firstright == s.newitemoff {
            V::New
        } else {
            V::Orig(sp.firstright.wrapping_sub(1))
        }
    };
    let firstright = |sp: &SplitPoint| {
        if !sp.newonleft && sp.firstright == s.newitemoff {
            V::New
        } else {
            V::Orig(sp.firstright)
        }
    };
    // _bt_strategy
    let mut strategy = Strategy::Default;
    let perfect = if !s.is_leaf {
        st.minfirstrightsz
    } else {
        let (li, ri) = interval_edges(&splits, interval);
        let p = keep(lastleft(&li), firstright(&ri));
        if p <= s.nkey {
            p
        } else {
            let p = keep(lastleft(&leftpage), firstright(&rightpage));
            if p <= s.nkey {
                strategy = Strategy::ManyDuplicates;
                s.nkey
            } else if s.is_rightmost {
                strategy = Strategy::SingleValue;
                p
            } else {
                let p = hikey_keep();
                if p <= s.nkey {
                    strategy = Strategy::SingleValue;
                }
                p
            }
        }
    };
    match strategy {
        Strategy::Default => {}
        Strategy::ManyDuplicates => interval = splits.len(),
        Strategy::SingleValue => {
            usemult = true;
            ffmult = SINGLEVAL_FILLFACTOR as f64 / 100.0;
            delta_sort(&mut splits, ffmult, usemult);
            interval = 1;
        }
    }
    // _bt_bestsplitloc
    let penalty = |sp: &SplitPoint| -> usize {
        if !s.is_leaf {
            if !sp.newonleft && sp.firstright == s.newitemoff {
                return newitemsz;
            }
            return s.sizes[sp.firstright] + ITEM_ID;
        }
        keep(lastleft(sp), firstright(sp))
    };
    let high = interval.min(splits.len());
    let mut best = usize::MAX;
    let mut low = 0;
    for (i, sp) in splits.iter().enumerate().take(high) {
        let p = penalty(sp);
        if p < best {
            best = p;
            low = i;
        }
        if p <= perfect {
            break;
        }
    }
    let mut fin = splits[low];
    if strategy == Strategy::ManyDuplicates
        && !s.is_rightmost
        && !fin.newonleft
        && fin.firstright >= s.newitemoff
        && fin.firstright < s.newitemoff + 9
    {
        fin = splits[0];
    }
    (fin.firstright, fin.newonleft)
}

fn delta_sort(splits: &mut [SplitPoint], ffmult: f64, usemult: bool) {
    for sp in splits.iter_mut() {
        let d = if usemult {
            (ffmult * sp.leftfree as f64 - (1.0 - ffmult) * sp.rightfree as f64) as i16
        } else {
            sp.leftfree - sp.rightfree
        };
        sp.curdelta = d.wrapping_abs();
    }
    pg_qsort(splits, &|a: &SplitPoint, b: &SplitPoint| a.curdelta.cmp(&b.curdelta));
}

fn default_interval(splits: &[SplitPoint], tolerance: i16) -> usize {
    let opt = splits[0];
    let (lowl, lowr) = (opt.leftfree - tolerance, opt.rightfree - tolerance);
    let (highl, highr) = (opt.leftfree + tolerance, opt.rightfree + tolerance);
    for (i, sp) in splits.iter().enumerate().skip(1) {
        if sp.leftfree < lowl || sp.rightfree < lowr || sp.leftfree > highl || sp.rightfree > highr
        {
            return i;
        }
    }
    splits.len()
}

fn interval_edges(splits: &[SplitPoint], interval: usize) -> (SplitPoint, SplitPoint) {
    let high = interval.min(splits.len());
    let opt = splits[0];
    let (mut l, mut r): (Option<SplitPoint>, Option<SplitPoint>) = (None, None);
    for i in (0..high).rev() {
        let d = splits[i];
        if d.firstright < opt.firstright {
            l.get_or_insert(d);
        } else if d.firstright > opt.firstright {
            r.get_or_insert(d);
        } else if !d.newonleft && opt.newonleft {
            l.get_or_insert(d);
        } else if d.newonleft && !opt.newonleft {
            r.get_or_insert(d);
        } else {
            l.get_or_insert(d);
            r.get_or_insert(d);
        }
        if l.is_some() && r.is_some() {
            break;
        }
    }
    (l.unwrap_or(opt), r.unwrap_or(opt))
}

/// Postgres's qsort (src/include/lib/sort_template.h), so that ties come out in the same
/// order.
pub fn pg_qsort<T: Copy>(a: &mut [T], cmp: &dyn Fn(&T, &T) -> Ordering) {
    let mut base = 0usize;
    let mut n = a.len();
    loop {
        let s = &mut a[base..base + n];
        if n < 7 {
            for m in 1..n {
                let mut l = m;
                while l > 0 && cmp(&s[l - 1], &s[l]) == Ordering::Greater {
                    s.swap(l, l - 1);
                    l -= 1;
                }
            }
            return;
        }
        if (1..n).all(|m| cmp(&s[m - 1], &s[m]) != Ordering::Greater) {
            return;
        }
        let med3 = |s: &[T], a: usize, b: usize, c: usize| -> usize {
            if cmp(&s[a], &s[b]) == Ordering::Less {
                if cmp(&s[b], &s[c]) == Ordering::Less {
                    b
                } else if cmp(&s[a], &s[c]) == Ordering::Less {
                    c
                } else {
                    a
                }
            } else if cmp(&s[b], &s[c]) == Ordering::Greater {
                b
            } else if cmp(&s[a], &s[c]) == Ordering::Less {
                a
            } else {
                c
            }
        };
        let mut pm = n / 2;
        if n > 7 {
            let mut pl = 0;
            let mut pn = n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(s, pl, pl + d, pl + 2 * d);
                pm = med3(s, pm - d, pm, pm + d);
                pn = med3(s, pn - 2 * d, pn - d, pn);
            }
            pm = med3(s, pl, pm, pn);
        }
        s.swap(0, pm);
        let (mut pa, mut pb) = (1usize, 1usize);
        let (mut pc, mut pd) = (n as isize - 1, n as isize - 1);
        loop {
            while pb as isize <= pc {
                let r = cmp(&s[pb], &s[0]);
                if r == Ordering::Greater {
                    break;
                }
                if r == Ordering::Equal {
                    s.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb as isize <= pc {
                let r = cmp(&s[pc as usize], &s[0]);
                if r == Ordering::Less {
                    break;
                }
                if r == Ordering::Equal {
                    s.swap(pc as usize, pd as usize);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb as isize > pc {
                break;
            }
            s.swap(pb, pc as usize);
            pb += 1;
            pc -= 1;
        }
        let pn = n;
        let d1 = pa.min(pb - pa);
        for i in 0..d1 {
            s.swap(i, pb - d1 + i);
        }
        let (pc, pd) = (pc as usize, pd as usize);
        let d1b = (pd - pc).min(pn - pd - 1);
        for i in 0..d1b {
            s.swap(pb + i, pn - d1b + i);
        }
        let d1 = pb - pa;
        let d2 = pd - pc;
        if d1 <= d2 {
            if d1 > 1 {
                pg_qsort(&mut a[base..base + d1], cmp);
            }
            if d2 > 1 {
                base = base + pn - d2;
                n = d2;
                continue;
            }
        } else {
            if d2 > 1 {
                pg_qsort(&mut a[base + pn - d2..base + pn], cmp);
            }
            if d1 > 1 {
                n = d1;
                continue;
            }
        }
        return;
    }
}

/// One level of a sorted build in progress (BTPageState).
struct Level {
    blk: u32,
    page: BPage,
    /// The page's low key (its downlink into the level above).
    lowkey: Option<Pivot>,
    last_truncextra: usize,
    full: usize,
}

struct Builder {
    tree: Btree,
    levels: Vec<Level>,
}

/// Space a page being built has left (PageGetFreeSpace, with the high key's line
/// pointer reserved).
fn build_free(p: &BPage) -> usize {
    let nlp = 1 + p.nitems();
    PAGE_SPACE
        .saturating_sub(p.used + ITEM_ID * nlp)
        .saturating_sub(ITEM_ID)
}

impl Builder {
    fn new_level(&mut self, level: usize) {
        let blk = self.tree.alloc(BPage::default());
        let full = if level == 0 {
            usize::MAX
        } else {
            BLCKSZ * (100 - NONLEAF_FILLFACTOR) / 100
        };
        self.levels.push(Level {
            blk,
            page: BPage {
                leaf: level == 0,
                ..Default::default()
            },
            lowkey: None,
            last_truncextra: 0,
            full,
        });
    }

    fn add_leaf(&mut self, it: Item, truncextra: usize, leaf_full: usize) {
        if self.levels.is_empty() {
            self.new_level(0);
            self.levels[0].full = leaf_full;
        }
        self.add(0, Entry::Leaf(it), truncextra);
    }

    /// _bt_buildadd.
    fn add(&mut self, level: usize, e: Entry, truncextra: usize) {
        let nkey = self.tree.opts.nkey;
        let isleaf = level == 0;
        let itupsz = match &e {
            Entry::Leaf(i) => i.size(),
            Entry::Inner(p, _) => p.size,
        };
        let st = &mut self.levels[level];
        let last_truncextra = st.last_truncextra;
        st.last_truncextra = truncextra;
        let pgspc = build_free(&st.page);
        let n = st.page.nitems();
        // A page keeps at least two items (one alone can't move to a new page).
        let min_items = if isleaf { 2 } else { 1 };
        if n >= min_items
            && (pgspc < itupsz + if isleaf { PIVOT_TID } else { 0 }
                || (pgspc + last_truncextra < st.full && n > 1))
        {
            // Finish the page: its last item moves to a new page and becomes (a
            // truncated copy of) this page's high key.
            let nblk = self.tree.alloc(BPage::default());
            let st = &mut self.levels[level];
            let mut newpage = BPage {
                leaf: isleaf,
                ..Default::default()
            };
            let high = if isleaf {
                let last = st.page.items.pop().expect("item");
                st.page.used -= last.size();
                let lastleft = st.page.items.last().expect("item");
                let k = lastleft.key.keep_natts(&last.key, nkey);
                let lastleft_tid = lastleft.tids.last();
                let high = if k <= nkey {
                    Pivot {
                        key: Some(last.key.clone()),
                        natts: k,
                        tid: None,
                        size: last.key.prefix_size(k),
                    }
                } else {
                    Pivot {
                        key: Some(last.key.clone()),
                        natts: nkey,
                        tid: Some(lastleft_tid),
                        size: last.key.prefix_size(nkey) + PIVOT_TID,
                    }
                };
                newpage.used += last.size();
                newpage.items.push(&last);
                high
            } else {
                let (pv, child) = st.page.inner.pop().expect("item");
                st.page.used -= pv.size;
                newpage.used += MINUS_INF;
                newpage.inner.push((Pivot::minus_inf(), child));
                pv
            };
            let oblk = st.blk;
            let mut old = std::mem::replace(&mut st.page, newpage);
            old.high = Some(high.clone());
            old.right = Some(nblk);
            st.blk = nblk;
            let lowkey = st.lowkey.replace(high).unwrap_or_else(Pivot::minus_inf);
            *self.tree.page_slot(oblk) = Some(old);
            if self.levels.len() == level + 1 {
                self.new_level(level + 1);
            }
            self.add(level + 1, Entry::Inner(lowkey, oblk), 0);
        }
        let st = &mut self.levels[level];
        if st.page.nitems() == 0 && st.lowkey.is_none() {
            st.lowkey = Some(Pivot::minus_inf());
        }
        match e {
            Entry::Leaf(i) => {
                st.page.used += i.size();
                st.page.items.push(&i);
            }
            Entry::Inner(p, child) => {
                let p = if st.page.inner.is_empty() {
                    Pivot::minus_inf()
                } else {
                    p
                };
                st.page.used += p.size;
                st.page.inner.push((p, child));
            }
        }
    }

    /// _bt_uppershutdown.
    fn finish(mut self) -> Btree {
        let mut i = 0;
        while i < self.levels.len() {
            let top = i + 1 == self.levels.len();
            let blk = self.levels[i].blk;
            if top {
                self.tree.root = Some(blk);
            } else {
                let lowkey = self.levels[i].lowkey.take().unwrap_or_else(Pivot::minus_inf);
                self.add(i + 1, Entry::Inner(lowkey, blk), 0);
            }
            let page = std::mem::take(&mut self.levels[i].page);
            *self.tree.page_slot(blk) = Some(page);
            i += 1;
        }
        self.tree.plain_leaves();
        self.tree
    }
}

enum Entry {
    Leaf(Item),
    Inner(Pivot, u32),
}

impl Btree {
    fn page_slot(&mut self, b: u32) -> &mut Option<BPage> {
        &mut self.pages[b as usize]
    }
}

// ── Page images ──
//
// A tree's pages as they are (items, posting lists, LP_DEAD marks, pivots, links), for a
// model kept across a restart of its database (`exec::sizing::kept`): PostgreSQL keeps its
// index pages, so the model does too. Varints; a key after the bytes it shares with the
// previous one on its page.

fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (a, rest) = (b.get(..n)?, b.get(n..)?);
    *b = rest;
    Some(a)
}

fn put_tid(out: &mut Vec<u8>, (blk, off): Tid) {
    put(out, blk as u64);
    put(out, off as u64);
}

fn get_tid(b: &mut &[u8]) -> Option<Tid> {
    Some((u32::try_from(get(b)?).ok()?, u16::try_from(get(b)?).ok()?))
}

fn put_pivot(out: &mut Vec<u8>, p: &Pivot) {
    put(out, p.key.is_some() as u64 | (p.tid.is_some() as u64) << 1);
    put(out, p.natts as u64);
    put(out, p.size as u64);
    if let Some(k) = &p.key {
        put(out, k.as_bytes().len() as u64);
        out.extend_from_slice(k.as_bytes());
    }
    if let Some(t) = p.tid {
        put_tid(out, t);
    }
}

fn get_pivot(b: &mut &[u8]) -> Option<Pivot> {
    let flags = get(b)?;
    let (natts, size) = (get(b)? as usize, get(b)? as usize);
    let key = if flags & 1 != 0 {
        let n = get(b)? as usize;
        Some(Key::from_bytes(take(b, n)?)?)
    } else {
        None
    };
    let tid = if flags & 2 != 0 { Some(get_tid(b)?) } else { None };
    Some(Pivot { key, natts, tid, size })
}

impl Btree {
    /// The tree's image (`read_image`), its options with it.
    pub fn write_image(&self, out: &mut Vec<u8>) {
        let o = &self.opts;
        put(out, o.nkey as u64);
        put(out, o.unique as u64 | (o.dedup as u64) << 1 | (o.nulls_not_distinct as u64) << 2);
        put(out, o.fillfactor as u64);
        put(out, self.pages.len() as u64);
        put(out, self.root.map_or(0, |r| r as u64 + 1));
        put(out, self.tuples as u64);
        for (b, p) in self.pages.iter().enumerate().skip(1) {
            let Some(p) = p else {
                // (A deleted page; 16: one the free space map has.)
                put(out, if self.fsm.as_ref().is_some_and(|f| f.records_free(b as u32)) { 16 } else { 0 });
                continue;
            };
            let flags = 1 | (p.leaf as u64) << 1 | (p.right.is_some() as u64) << 2 | (p.high.is_some() as u64) << 3;
            put(out, flags);
            if let Some(r) = p.right {
                put(out, r as u64);
            }
            if let Some(h) = &p.high {
                put_pivot(out, h);
            }
            put(out, p.used as u64);
            if !p.leaf {
                put(out, p.inner.len() as u64);
                for (pv, child) in &p.inner {
                    put_pivot(out, pv);
                    put(out, *child as u64);
                }
                continue;
            }
            put(out, p.items.len() as u64);
            let mut prev: &[u8] = &[];
            for it in p.items.iter() {
                let tids = it.tids;
                // (Bit 2: an explicit size follows. Sizes are always derived now.)
                put(out, it.dead as u64);
                let k = it.key.as_bytes();
                let shared = prev.iter().zip(k).take_while(|(a, b)| a == b).count();
                put(out, shared as u64);
                put(out, (k.len() - shared) as u64);
                out.extend_from_slice(&k[shared..]);
                prev = k;
                put(out, tids.len() as u64);
                for t in tids.iter() {
                    put_tid(out, t);
                }
            }
        }
    }

    /// A tree from `write_image`'s bytes (advanced past them); None when they don't hold
    /// one.
    pub fn read_image(b: &mut &[u8]) -> Option<Btree> {
        let nkey = get(b)? as usize;
        let flags = get(b)?;
        let fillfactor = get(b)? as usize;
        let opts = BtreeOpts { nkey, unique: flags & 1 != 0, nulls_not_distinct: flags & 4 != 0, dedup: flags & 2 != 0, fillfactor };
        let n = get(b)? as usize;
        if n == 0 || n > 1 << 24 {
            return None;
        }
        let blk = |v: u64| u32::try_from(v).ok().filter(|&x| x >= 1 && (x as usize) < n);
        let root = match get(b)? {
            0 => None,
            r => Some(blk(r - 1)?),
        };
        let tuples = get(b)? as usize;
        let mut pages = Vec::with_capacity(n);
        pages.push(None);
        let mut free = Vec::new();
        for i in 1..n {
            let flags = get(b)?;
            if flags & 1 == 0 {
                if flags & 16 != 0 {
                    free.push(i as u32);
                }
                pages.push(None);
                continue;
            }
            let leaf = flags & 2 != 0;
            let right = if flags & 4 != 0 { Some(blk(get(b)?)?) } else { None };
            let high = if flags & 8 != 0 { Some(get_pivot(b)?) } else { None };
            let used = get(b)? as usize;
            let mut p = BPage { leaf, right, high, used, ..Default::default() };
            let m = get(b)? as usize;
            if !leaf {
                for _ in 0..m {
                    let pv = get_pivot(b)?;
                    p.inner.push((pv, blk(get(b)?)?));
                }
            } else {
                let mut prev: Vec<u8> = Vec::new();
                let mut tids: Vec<Tid> = Vec::new();
                for _ in 0..m {
                    let f = get(b)?;
                    let shared = get(b)? as usize;
                    let len = get(b)? as usize;
                    let mut k = Vec::with_capacity(shared + len);
                    k.extend_from_slice(prev.get(..shared)?);
                    k.extend_from_slice(take(b, len)?);
                    if !Key::well_formed(&k) {
                        return None;
                    }
                    let nt = get(b)? as usize;
                    if nt == 0 || nt >= 1 << 16 {
                        return None;
                    }
                    tids.clear();
                    for _ in 0..nt {
                        tids.push(get_tid(b)?);
                    }
                    // (An explicit size: never other than the derived one, which is used.)
                    if f & 2 != 0 {
                        get(b)?;
                    }
                    p.items.push_parts(f & 1 != 0, &tids, &k);
                    prev = k;
                }
            }
            pages.push(Some(p));
        }
        let mut t = Btree { pages, root, tuples, ..Btree::new(opts) };
        // (Deleted pages: those in the free space map back in it; the rest wait for a
        // VACUUM, recyclable by then.)
        let deleted = t.pages.iter().skip(1).filter(|p| p.is_none()).count() as u32;
        if !free.is_empty() {
            let f = t.fsm.get_or_insert_default();
            for &blk in &free {
                f.record_free_index_page(blk);
            }
            f.vacuum_all();
        }
        t.pending_delpages = deleted - free.len() as u32;
        t.plain_leaves();
        Some(t)
    }

    /// Every leaf kept plain that can be (`leaf::Leaf::plain`): a build's, a layout's.
    fn plain_leaves(&mut self) {
        for p in self.pages.iter_mut().flatten() {
            if p.leaf {
                p.items.plain();
            }
        }
    }
}

// (A model holds one per entry / version.)
const _: () = assert!(size_of::<Item>() == 32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_keys_follow_tags_not_encoding_length() {
        use crate::types::{Value, numeric::Numeric};
        let values = [
            Value::Null,
            Value::Numeric(Numeric::zero().into()),
            Value::Numeric(Numeric::NegInf.into()),
            Value::Numeric(Numeric::PosInf.into()),
            Value::Numeric(Numeric::NaN.into()),
        ];
        for value in values {
            for desc in [false, true] {
                for nulls_first in [false, true] {
                    let mut encoded = Vec::new();
                    crate::storage::keys::encode_index_value(&mut encoded, &value, desc, nulls_first, None);
                    let key = Key::new(&[encoded], &[16]);
                    assert_eq!(key.has_null(1), value.is_null(), "{value:?} desc={desc} nulls_first={nulls_first}");
                }
            }
        }
    }

    #[test]
    fn packed_tree_keeps_large_null_keys_across_restart() {
        let opts = BtreeOpts { nkey: 32, unique: false, nulls_not_distinct: false, dedup: false, fillfactor: 90 };
        let mut b = Btree::new(opts);
        let hint = InsertHint { unchanged: false, dead: &|_| false };
        for i in 0..800 {
            b.insert(Key::new(&vec![vec![0]; 32], &[16; 32]), (i / 226, (i % 226 + 1) as u16), &hint);
        }
        // (Laid out item by item, as a page with an LP_DEAD item or a posting list is: its
        // items' places past 64 KiB. A plain page has none.)
        for p in b.pages.iter_mut().flatten() {
            p.items.unplain();
        }
        assert!(b.pages.iter().flatten().any(|p| p.items.bytes() > u16::MAX as usize));
        let mut image = Vec::new();
        b.write_image(&mut image);
        let mut input = image.as_slice();
        let mut restored = Btree::read_image(&mut input).unwrap();
        assert!(input.is_empty());
        let mut again = Vec::new();
        restored.write_image(&mut again);
        assert_eq!(image, again);
        let dead = (0..300).map(|i| (i / 226, (i % 226 + 1) as u16)).collect();
        assert_eq!(restored.bulk_delete(&dead), 300);
        assert_eq!(restored.tuples, 500);
    }

    #[test]
    fn qsort_matches_sorted() {
        let mut v: Vec<i32> = (0..500).map(|i| (i * 7919) % 101).collect();
        pg_qsort(&mut v, &|a: &i32, b: &i32| a.cmp(b));
        let mut w = v.clone();
        w.sort();
        assert_eq!(v, w);
    }

    /// A size watch holds until the index gains a page, and fails for good once the model goes.
    #[test]
    fn size_watch_follows_pages() {
        let opts = BtreeOpts { nkey: 1, unique: true, nulls_not_distinct: false, dedup: false, fillfactor: 90 };
        let mut b = Btree::new(opts);
        let hint = InsertHint { unchanged: false, dead: &|_| false };
        let key = |i: u32| Key::new(&[i.to_be_bytes().to_vec()], &[16]);
        b.insert(key(0), (0, 1), &hint);
        let w = b.sizes.watch();
        let pages = b.nblocks();
        let mut i = 1;
        while b.nblocks() == pages {
            assert!(w.holds(), "no page added yet ({i} entries)");
            b.insert(key(i), (i / 200, (i % 200) as u16 + 1), &hint);
            i += 1;
        }
        assert!(!w.holds());
        let w = b.sizes.watch();
        b.insert(key(i), (i / 200, 1), &hint);
        assert!(w.holds());
        drop(b);
        assert!(!w.holds());
    }

    /// Keys of any length (a 100 kB text key, many attributes of it) fit the key's u16
    /// offsets: equal values give equal keys, values that differ past their kept bytes
    /// still differ, and sizes past u16 are capped (no panic, no wrapped length).
    #[test]
    fn long_keys_fit() {
        let big = |c: u8, n: usize| vec![c; n];
        let a = Key::new(&[big(7, 100_000), big(8, 70_000), big(9, 10)], &[2712, 70_016, 80_000]);
        let b = Key::new(&[big(7, 100_000), big(8, 70_000), big(9, 10)], &[2712, 70_016, 80_000]);
        let mut other = big(7, 100_000);
        other[99_999] = 6;
        let c = Key::new(&[other, big(8, 70_000), big(9, 10)], &[2712, 70_016, 80_000]);
        assert!(a.as_bytes().len() < 1 << 16);
        assert_eq!(a.as_bytes(), b.as_bytes());
        assert_ne!(a.as_bytes(), c.as_bytes());
        assert_eq!(a.size(), u16::MAX as usize);
        let many: Vec<Vec<u8>> = (0..32).map(|i| big(i, 5_000)).collect();
        let k = Key::new(&many, &[64; 32]);
        assert_eq!(Key::from_bytes(k.as_bytes()).map(|x| x.as_bytes().to_vec()), Some(k.as_bytes().to_vec()));
    }

    /// VACUUM deletes the leaves it empties (and parents left without children) the way
    /// _bt_pagedel does: afterwards every downlink and sibling link leads to a live page,
    /// each level's chain holds its pages in key order, every remaining entry is found by
    /// a descent, the deleted blocks are counted and kept, and inserts into the deleted
    /// key space go to the right sibling that took it over.
    #[test]
    fn vacuum_deletes_emptied_pages() {
        let opts = BtreeOpts { nkey: 1, unique: true, nulls_not_distinct: false, dedup: false, fillfactor: 90 };
        let mut b = Btree::new(opts);
        let hint = InsertHint { unchanged: false, dead: &|_| false };
        // (Wide keys: three levels from a few thousand entries.)
        let key = |i: u32| {
            let mut a = i.to_be_bytes().to_vec();
            a.resize(300, 7);
            Key::new(&[a], &[312])
        };
        let tid = |i: u32| (i / 20, (i % 20) as u16 + 1);
        for i in 0..6000 {
            b.insert(key(i), tid(i), &hint);
        }
        assert!(b.height() >= 2, "three levels");
        let check = |b: &Btree, present: &dyn Fn(u32) -> bool| {
            for (blk, p) in b.pages.iter().enumerate() {
                let Some(p) = p else { continue };
                for (_, c) in &p.inner {
                    assert!(b.pages[*c as usize].is_some(), "downlink from {blk} to a deleted page");
                }
                if let Some(r) = p.right {
                    assert!(b.pages[r as usize].is_some(), "right link from {blk} to a deleted page");
                }
            }
            // The leaf level from its leftmost page: every live leaf once, keys ascending.
            let mut blk = b.root.expect("root");
            while !b.pages[blk as usize].as_ref().expect("page").leaf {
                blk = b.pages[blk as usize].as_ref().expect("page").inner[0].1;
            }
            let (mut seen, mut keys) = (0, Vec::new());
            let mut cur = Some(blk);
            while let Some(c) = cur {
                let p = b.pages[c as usize].as_ref().expect("page");
                seen += 1;
                keys.extend(p.items.iter().map(|it| it.key.as_bytes().to_vec()));
                cur = p.right;
            }
            assert_eq!(seen, b.pages.iter().flatten().filter(|p| p.leaf).count(), "leaves on the chain");
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys in order");
            for i in 0..9000 {
                let k = key(i);
                assert_eq!(b.postings(k.key_bytes(1)).is_empty(), !present(i), "entry {i} found by a descent");
            }
            let s = b.page_stats();
            assert_eq!(s.deleted as usize, b.pages.iter().skip(1).filter(|p| p.is_none()).count());
        };
        // Most of the start, a stretch in the middle, a few scattered leaves.
        let gone = |i: u32| i < 2500 || (3000..4200).contains(&i) || (i / 10) % 37 == 3;
        let dead: crate::exec::fasthash::HashSet<Tid> = (0..6000).filter(|&i| gone(i)).map(tid).collect();
        let blocks = b.nblocks();
        b.bulk_delete(&dead);
        assert_eq!(b.nblocks(), blocks, "deleted blocks stay in the file");
        let deleted = b.page_stats().deleted;
        assert!(deleted > 50, "pages deleted: {deleted}");
        check(&b, &|i| i < 6000 && !gone(i));
        // Back into the deleted key space, and past the end.
        for i in (0..2500).step_by(3).chain(6000..9000) {
            b.insert(key(i), tid(i), &hint);
        }
        check(&b, &|i| (i < 6000 && !gone(i)) || (i < 2500 && i % 3 == 0) || (6000..9000).contains(&i));
        assert_eq!(b.page_stats().deleted, deleted, "deleted pages aren't reused before a VACUUM frees them");
        assert_eq!(b.fsm_blocks(), 0);
        // A VACUUM once every transaction is past their safexid: into the free space map,
        // which splits then take their pages from instead of extending the file.
        assert!(b.needs_cleanup());
        b.vacuum_scan(None, &|_| true, 0);
        assert_eq!(b.fsm_blocks(), 3, "the free space map's three levels");
        assert!(!b.needs_cleanup());
        // (Kept across a restart: the map's pages, the deleted pages in it.)
        let mut img = Vec::new();
        b.write_image(&mut img);
        let mut b = Btree::read_image(&mut &img[..]).expect("image");
        assert_eq!((b.fsm_blocks(), b.page_stats().deleted), (3, deleted));
        let blocks = b.nblocks();
        for i in (3000..4200).step_by(2) {
            b.insert(key(i), tid(i), &hint);
        }
        let reused = deleted - b.page_stats().deleted;
        assert!(reused > 0, "pages reused");
        assert_eq!(b.nblocks(), blocks, "no new pages while deleted ones are free");
        check(&b, &|i| {
            (i < 6000 && !gone(i)) || (i < 2500 && i % 3 == 0) || (6000..9000).contains(&i) || ((3000..4200).contains(&i) && i % 2 == 0)
        });
    }

    /// A queue's index: entries added at the end, the head's killed as scans from the start
    /// pass them. Walks starting past the dead head (`DeadHead`) mark and return what a
    /// walk from the leftmost leaf does, with an upper bound or not, as the head grows, and
    /// after an insertion into it or a VACUUM.
    #[test]
    fn dead_head_walks_match_full_walks() {
        let opts = BtreeOpts { nkey: 1, unique: true, nulls_not_distinct: false, dedup: false, fillfactor: 90 };
        let (mut a, mut b) = (Btree::new(opts.clone()), Btree::new(opts));
        let hint = InsertHint { unchanged: false, dead: &|_| false };
        let key = |i: u32| Key::new(&[i.to_be_bytes().to_vec()], &[16]);
        let tid = |i: u32| (i / 200, (i % 200) as u16 + 1);
        let marks = |t: &Btree| -> Vec<bool> {
            t.pages.iter().flatten().filter(|p| p.leaf).flat_map(|p| p.items.iter().map(|it| it.dead).collect::<Vec<_>>()).collect()
        };
        let (mut next, mut head) = (0u32, 0u32);
        for round in 0..300u32 {
            for _ in 0..60 {
                a.insert(key(next), tid(next), &hint);
                b.insert(key(next), tid(next), &hint);
                next += 1;
            }
            head = (head + 50).min(next);
            let dead = |t: Tid| t < tid(head);
            if round % 37 == 5 {
                // (Back into the head: a key below every live one.)
                a.insert(key(head / 2), (u32::MAX, 1), &hint);
                b.insert(key(head / 2), (u32::MAX, 1), &hint);
            }
            if round % 101 == 77 {
                let gone: crate::exec::fasthash::HashSet<Tid> = (0..head / 2).map(tid).collect();
                a.bulk_delete(&gone);
                b.bulk_delete(&gone);
            }
            let hi = (round % 3 == 0).then(|| (head + 7).to_be_bytes().to_vec());
            b.head.forget();
            let (mut sa, mut sb) = (Vec::new(), Vec::new());
            assert_eq!(
                a.kill_needed(None, hi.as_deref(), &dead, Some(&mut sa)),
                b.kill_needed(None, hi.as_deref(), &dead, Some(&mut sb))
            );
            assert_eq!(sa, sb, "kill_needed's entries, round {round}");
            b.head.forget();
            let (mut sa, mut sb) = (Vec::new(), Vec::new());
            a.kill(None, hi.as_deref(), &dead, Some(&mut sa));
            b.kill(None, hi.as_deref(), &dead, Some(&mut sb));
            assert_eq!(sa, sb, "kill's entries, round {round}");
            assert_eq!(marks(&a), marks(&b), "marks, round {round}");
        }
        assert!(a.head.0.load(std::sync::atomic::Ordering::Relaxed) != 0, "a dead head was found");
    }
}
