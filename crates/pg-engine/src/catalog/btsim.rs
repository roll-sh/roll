//! A model of a btree index file as Postgres builds and grows it: a metapage, leaf pages
//! filled to the fillfactor by a sorted build (with deduplication into posting lists and
//! suffix-truncated high keys), page splits at the point nbtsplitloc.c would choose,
//! deduplication passes that put off splits, and new levels as the root splits.

use super::heapsim::{BLCKSZ, ITEM_ID, PAGE_HEADER, Tid, get_varint as get, maxalign, put_varint as put};
use std::cmp::Ordering;
use std::sync::Arc;

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
#[derive(Clone, Debug)]
pub struct Key {
    buf: Box<[u8]>,
}

impl Key {
    /// Its one allocation as it is (`sizing`'s models kept across a restart).
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// A key from `as_bytes`, when they hold one.
    pub fn from_bytes(buf: Vec<u8>) -> Option<Key> {
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
        (data <= buf.len() && data + prev == buf.len()).then(|| Key { buf: buf.into() })
    }

    /// `atts`: per-attribute encodings; `sizes[k]`: the MAXALIGNed index tuple size
    /// holding the first k+1 attributes.
    pub fn new(atts: &[Vec<u8>], sizes: &[usize]) -> Key {
        let len: usize = atts.iter().map(Vec::len).sum();
        let mut buf = Vec::with_capacity(2 + 2 * (atts.len() + sizes.len()) + len);
        buf.push(atts.len() as u8);
        buf.push(sizes.len() as u8);
        let mut end = 0u16;
        for a in atts {
            end += a.len() as u16;
            buf.extend_from_slice(&end.to_le_bytes());
        }
        for &z in sizes {
            buf.extend_from_slice(&(z as u16).to_le_bytes());
        }
        for a in atts {
            buf.extend_from_slice(a);
        }
        Key { buf: buf.into() }
    }

    fn u16_at(&self, i: usize) -> usize {
        u16::from_le_bytes([self.buf[2 + 2 * i], self.buf[3 + 2 * i]]) as usize
    }

    fn natts(&self) -> usize {
        self.buf[0] as usize
    }

    fn nsizes(&self) -> usize {
        self.buf[1] as usize
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
        &self.buf[d + start..d + self.end(a)]
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
        &self.buf[d..d + if n == 0 { 0 } else { self.end(n - 1) }]
    }

    fn cmp_keys(&self, o: &Key, nkey: usize) -> Ordering {
        for a in 0..nkey {
            match self.seg(a).cmp(o.seg(a)) {
                Ordering::Equal => {}
                x => return x,
            }
        }
        Ordering::Equal
    }

    /// _bt_keep_natts_fast: the first key attribute that differs (1-based), or nkey + 1.
    fn keep_natts(&self, o: &Key, nkey: usize) -> usize {
        for a in 0..nkey {
            if self.seg(a) != o.seg(a) {
                return a + 1;
            }
        }
        nkey + 1
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
    key: Arc<Key>,
    /// Heap TIDs, ascending (more than one: a posting list tuple).
    tids: Tids,
    size: usize,
    /// LP_DEAD: an index scan found every heap tuple it points at dead to everyone
    /// (_bt_killitems).
    dead: bool,
}

impl Item {
    fn plain(key: Arc<Key>, tid: Tid) -> Item {
        let size = key.size();
        Item {
            key,
            tids: Tids::One(tid),
            size,
            dead: false,
        }
    }

    fn posting(key: Arc<Key>, tids: impl Into<Tids>) -> Item {
        let tids = tids.into();
        let size = if tids.len() > 1 {
            maxalign(key.size() + 6 * tids.len())
        } else {
            key.size()
        };
        Item {
            key,
            tids,
            size,
            dead: false,
        }
    }

    fn posting_bytes(&self) -> usize {
        self.size - self.key.size()
    }
}

#[derive(Clone, Debug)]
struct Pivot {
    key: Option<Arc<Key>>,
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
fn above(it: &Item, lo: &[u8], nkey: usize) -> bool {
    let k = it.key.key_bytes(nkey);
    let m = k.len().min(lo.len());
    k[..m] >= lo[..m]
}

/// The first of a leaf's items (in key order) past `lo`. A binary search: the key
/// attributes' encodings are self-delimiting, so no item past `lo` is followed by one
/// that isn't.
fn first_above(items: &[Item], lo: &[u8], nkey: usize) -> usize {
    let i = items.partition_point(|it| !above(it, lo, nkey));
    debug_assert!(items[i..].iter().all(|it| above(it, lo, nkey)), "items out of order");
    i
}

#[derive(Clone, Debug, Default)]
struct BPage {
    leaf: bool,
    items: Vec<Item>,
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

/// What `Btree::page_stats` counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct BtPageStats {
    pub root: u32,
    pub internal: u64,
    pub leaf: u64,
    pub empty: u64,
    pub leaf_free: u64,
    pub fragments: u64,
    pub items: u64,
    pub item_bytes: u64,
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
}

impl Btree {
    pub fn new(opts: BtreeOpts) -> Btree {
        Btree {
            opts,
            pages: vec![None],
            root: None,
            tuples: 0,
        }
    }

    pub fn nblocks(&self) -> u32 {
        self.pages.len() as u32
    }

    /// Every leaf entry's key and heap TID (each TID of a posting list), in index order.
    pub fn entries(&self) -> impl Iterator<Item = (&Arc<Key>, Tid)> {
        let leaves = self.leaf_pages(None, None).into_iter().filter_map(|b| self.pages.get(b as usize)?.as_ref());
        leaves.flat_map(|p| p.items.iter()).flat_map(|it| it.tids.iter().map(move |t| (&it.key, *t)))
    }

    /// pgstatindex's walk over the pages after the metapage: (root block, internal pages,
    /// leaf pages, empty leaf pages, leaf free bytes (PageGetExactFreeSpace), leaf
    /// fragments: leaves whose right sibling comes before them) and pgstattuple's over all
    /// of them: (items, their bytes, free bytes).
    pub fn page_stats(&self) -> BtPageStats {
        let mut s = BtPageStats { root: self.root.unwrap_or(0), ..Default::default() };
        for (b, p) in self.pages.iter().enumerate().skip(1) {
            let Some(p) = p else { continue };
            let free = p.free() + ITEM_ID;
            s.free += free as u64;
            s.items += p.nitems() as u64;
            s.item_bytes += (p.used + ITEM_ID * p.nitems()) as u64;
            if !p.leaf {
                s.internal += 1;
            } else if p.items.is_empty() {
                s.empty += 1;
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
        self.pages.push(Some(p));
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
    pub fn build(opts: BtreeOpts, mut entries: Vec<(Arc<Key>, Tid)>) -> Btree {
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
            let mut pending: Option<(Arc<Key>, Vec<Tid>)> = None;
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
    pub fn insert(&mut self, key: Arc<Key>, tid: Tid, hint: &InsertHint) {
        self.tuples += 1;
        let Some(root) = self.root else {
            let b = self.alloc(BPage {
                leaf: true,
                ..Default::default()
            });
            self.root = Some(b);
            let p = self.page(b);
            let it = Item::plain(key, tid);
            p.used += it.size;
            p.items.push(it);
            return;
        };
        // Descend, remembering the path.
        let mut stack: Vec<(u32, usize)> = Vec::new();
        let mut blk = root;
        loop {
            let p = self.pages[blk as usize].as_ref().expect("btree page");
            if p.leaf {
                break;
            }
            let i = p
                .inner
                .partition_point(|(pv, _)| self.cmp_pivot(&key, tid, pv) == Ordering::Greater)
                .saturating_sub(1);
            stack.push((blk, i));
            blk = p.inner[i].1;
        }
        let nkey = self.opts.nkey;
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        let pos = p.items.partition_point(|it| {
            match it.key.cmp_keys(&key, nkey) {
                Ordering::Less => true,
                Ordering::Greater => false,
                Ordering::Equal => it.tids[0] < tid,
            }
        });
        let new = Item::plain(key.clone(), tid);
        let itemsz = new.size;
        let uniquedup = self.opts.unique
            && [pos.checked_sub(1), Some(pos)]
                .into_iter()
                .flatten()
                .filter_map(|i| p.items.get(i))
                .any(|it| it.key.keep_natts(&key, nkey) > nkey);
        if p.free() < itemsz {
            self.delete_or_dedup(blk, &key, itemsz, uniquedup, hint);
        }
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        let pos = p.items.partition_point(|it| {
            match it.key.cmp_keys(&key, nkey) {
                Ordering::Less => true,
                Ordering::Greater => false,
                Ordering::Equal => it.tids[0] < tid,
            }
        });
        if p.free() >= itemsz {
            let p = self.page(blk);
            p.used += itemsz;
            p.items.insert(pos, new);
            return;
        }
        self.split_leaf(blk, pos, new, stack);
    }

    /// _bt_delete_or_dedup_one_page: bottom-up deletion when an UPDATE's duplicates pile
    /// up, then a deduplication pass.
    fn delete_or_dedup(
        &mut self,
        blk: u32,
        key: &Key,
        itemsz: usize,
        uniquedup: bool,
        hint: &InsertHint,
    ) {
        // Simple deletion of the LP_DEAD items first (_bt_simpledel_pass), and of the
        // entries pointing into the same heap blocks that are dead too.
        let mut uniquedup = uniquedup;
        let p = self.page(blk);
        if p.items.iter().any(|it| it.dead) {
            let blocks: std::collections::HashSet<u32> = p
                .items
                .iter()
                .filter(|it| it.dead)
                .flat_map(|it| it.tids.iter().map(|t| t.0))
                .collect();
            let mut kept = Vec::with_capacity(p.items.len());
            for mut it in std::mem::take(&mut p.items) {
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
            p.items = kept;
            p.used = p.items.iter().map(|i| i.size).sum();
            if p.free() >= itemsz {
                return;
            }
            uniquedup = true;
        }
        if self.opts.unique && !uniquedup {
            return;
        }
        let bottomup = hint.unchanged || uniquedup;
        if bottomup {
            let p = self.page(blk);
            let mut freed = 0;
            let mut kept = Vec::with_capacity(p.items.len());
            for mut it in std::mem::take(&mut p.items) {
                let before = it.size;
                it.tids.retain(|t| !(hint.dead)(*t));
                if it.tids.is_empty() {
                    freed += before + ITEM_ID;
                    continue;
                }
                let it = Item::posting(it.key, it.tids);
                freed += before - it.size;
                kept.push(it);
            }
            p.items = kept;
            p.used = p.items.iter().map(|i| i.size).sum();
            let _ = freed;
            if p.free() + ITEM_ID >= (BLCKSZ / 16).max(itemsz) {
                return;
            }
        }
        if self.opts.dedup {
            self.dedup_pass(blk, key, itemsz, bottomup);
        }
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
            && newkey.keep_natts(&p.items[0].key, nkey) > nkey
            && newkey.keep_natts(&p.items[p.items.len() - 1].key, nkey) > nkey;
        let mut deduplicate = true;
        let mut nmaxitems = 0;
        let mut nintervals = 0;
        let mut out: Vec<Item> = Vec::with_capacity(p.items.len());
        let mut pending: Option<(Arc<Key>, Vec<Tid>, usize)> = None;
        let finish = |pending: Option<(Arc<Key>, Vec<Tid>, usize)>,
                      out: &mut Vec<Item>,
                      nintervals: &mut usize| {
            if let Some((k, tids, nitems)) = pending {
                if nitems > 1 {
                    *nintervals += 1;
                }
                out.push(Item::posting(k, tids));
            }
        };
        for it in std::mem::take(&mut p.items) {
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
        p.used = out.iter().map(|i| i.size).sum();
        p.items = out;
    }

    /// _bt_split on a leaf, then the downlink into the parent.
    fn split_leaf(&mut self, blk: u32, pos: usize, new: Item, stack: Vec<(u32, usize)>) {
        let nkey = self.opts.nkey;
        let fillfactor = self.opts.fillfactor;
        let p = self.pages[blk as usize].as_ref().expect("btree page");
        let rightmost = p.right.is_none();
        let (firstright, newonleft) = {
            let at = |v: V| -> &Item {
                match v {
                    V::New => &new,
                    V::Orig(i) => &p.items[i],
                }
            };
            let keep = |a: V, b: V| at(a).key.keep_natts(&at(b).key, nkey);
            let afternew = |s: &SplitState| -> Option<bool> {
                // _bt_afternewitemoff.
                if nkey == 1 || s.newitemoff == 0 {
                    return None;
                }
                let n = p.items.len();
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
                let prev = &p.items[s.newitemoff - 1];
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
                sizes: p.items.iter().map(|i| i.size).collect(),
                posting: p.items.iter().map(|i| i.posting_bytes()).collect(),
                newitemoff: pos,
                newitemsz: new.size,
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
        let mut all: Vec<Item> = std::mem::take(&mut self.page(blk).items);
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
        let right = BPage {
            leaf: true,
            used: right_items.iter().map(|i| i.size).sum(),
            items: right_items,
            inner: Vec::new(),
            high: old_high,
            right: old_right,
        };
        let rblk = self.alloc(right);
        let left = self.page(blk);
        left.used = all.iter().map(|i| i.size).sum();
        left.items = all;
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
                items: Vec::new(),
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
            items: Vec::new(),
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
            for it in &p.items {
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
        let nkey = self.opts.nkey;
        let Some(mut blk) = self.root else {
            return;
        };
        // Down to the leaf where `lo` starts (pivots compare as their kept attributes).
        loop {
            let Some(p) = self.pages.get(blk as usize).and_then(|p| p.as_ref()) else {
                return;
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
        // Then right along the leaves until past `hi`. (A storage key goes on with the
        // row's key: an entry is past `lo` when some row key puts it there.)
        // (Once a leaf has an item past `lo`, every item after it is.)
        let mut lo = lo;
        loop {
            let Some(p) = self.pages.get_mut(blk as usize).and_then(|p| p.as_mut()) else {
                return;
            };
            let from = lo.map_or(0, |l| first_above(&p.items, l, nkey));
            if from < p.items.len() {
                lo = None;
            }
            for it in &mut p.items[from..] {
                let k = it.key.key_bytes(nkey);
                if hi.is_some_and(|hi| k >= hi) {
                    return;
                }
                if !it.dead
                    && let Some(seen) = seen.as_deref_mut()
                {
                    seen.extend_from_slice(&it.tids);
                }
                if !it.dead && it.tids.iter().all(|t| all_dead(*t)) {
                    it.dead = true;
                }
            }
            match p.right {
                Some(r) => blk = r,
                None => return,
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
        // Then right along the leaves until past `hi`. (A storage key goes on with the
        // row's key: an entry is past `lo` when some row key puts it there.)
        let mut lo = lo;
        loop {
            let Some(p) = self.pages.get(blk as usize).and_then(|p| p.as_ref()) else {
                return false;
            };
            let from = lo.map_or(0, |l| first_above(&p.items, l, nkey));
            if from < p.items.len() {
                lo = None;
            }
            for it in &p.items[from..] {
                let k = it.key.key_bytes(nkey);
                if hi.is_some_and(|hi| k >= hi) {
                    return false;
                }
                if !it.dead
                    && let Some(seen) = seen.as_deref_mut()
                {
                    seen.extend_from_slice(&it.tids);
                }
                if !it.dead && it.tids.iter().all(|t| all_dead(*t)) {
                    return true;
                }
            }
            match p.right {
                Some(r) => blk = r,
                None => return false,
            }
        }
    }

    /// VACUUM's bulk delete: entries pointing at removed heap tuples go (pages stay).
    pub fn bulk_delete(&mut self, dead: &std::collections::HashSet<Tid>) -> usize {
        let mut removed = 0;
        for p in self.pages.iter_mut().flatten() {
            if !p.leaf {
                continue;
            }
            let mut kept = Vec::with_capacity(p.items.len());
            for mut it in std::mem::take(&mut p.items) {
                let before = it.tids.len();
                it.tids.retain(|t| !dead.contains(t));
                removed += before - it.tids.len();
                if !it.tids.is_empty() {
                    kept.push(Item::posting(it.key, it.tids));
                }
            }
            p.used = kept.iter().map(|i| i.size).sum();
            p.items = kept;
        }
        self.tuples = self.tuples.saturating_sub(removed);
        removed
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
            Entry::Leaf(i) => i.size,
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
                st.page.used -= last.size;
                let lastleft = st.page.items.last().expect("item");
                let k = lastleft.key.keep_natts(&last.key, nkey);
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
                        tid: lastleft.tids.last().copied(),
                        size: last.key.prefix_size(nkey) + PIVOT_TID,
                    }
                };
                newpage.used += last.size;
                newpage.items.push(last);
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
                st.page.used += i.size;
                st.page.items.push(i);
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
        Some(Arc::new(Key::from_bytes(take(b, n)?.to_vec())?))
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
        put(out, o.unique as u64 | (o.dedup as u64) << 1);
        put(out, o.fillfactor as u64);
        put(out, self.pages.len() as u64);
        put(out, self.root.map_or(0, |r| r as u64 + 1));
        put(out, self.tuples as u64);
        for p in self.pages.iter().skip(1) {
            let Some(p) = p else {
                put(out, 0);
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
            for it in &p.items {
                let tids: &[Tid] = &it.tids;
                let derived = Item::posting(it.key.clone(), tids.to_vec()).size;
                put(out, it.dead as u64 | ((it.size != derived) as u64) << 1);
                let k = it.key.as_bytes();
                let shared = prev.iter().zip(k).take_while(|(a, b)| a == b).count();
                put(out, shared as u64);
                put(out, (k.len() - shared) as u64);
                out.extend_from_slice(&k[shared..]);
                prev = k;
                put(out, tids.len() as u64);
                for &t in tids {
                    put_tid(out, t);
                }
                if it.size != derived {
                    put(out, it.size as u64);
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
        let opts = BtreeOpts { nkey, unique: flags & 1 != 0, dedup: flags & 2 != 0, fillfactor };
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
        for _ in 1..n {
            let flags = get(b)?;
            if flags & 1 == 0 {
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
                for _ in 0..m {
                    let f = get(b)?;
                    let shared = get(b)? as usize;
                    let len = get(b)? as usize;
                    let mut k = Vec::with_capacity(shared + len);
                    k.extend_from_slice(prev.get(..shared)?);
                    k.extend_from_slice(take(b, len)?);
                    let key = Arc::new(Key::from_bytes(k.clone())?);
                    prev = k;
                    let nt = get(b)? as usize;
                    if nt == 0 || nt > 1 << 16 {
                        return None;
                    }
                    let tids: Vec<Tid> = (0..nt).map(|_| get_tid(b)).collect::<Option<_>>()?;
                    let mut it = Item::posting(key, tids);
                    it.dead = f & 1 != 0;
                    if f & 2 != 0 {
                        it.size = get(b)? as usize;
                    }
                    p.items.push(it);
                }
            }
            pages.push(Some(p));
        }
        Some(Btree { opts, pages, root, tuples })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qsort_matches_sorted() {
        let mut v: Vec<i32> = (0..500).map(|i| (i * 7919) % 101).collect();
        pg_qsort(&mut v, &|a: &i32, b: &i32| a.cmp(b));
        let mut w = v.clone();
        w.sort();
        assert_eq!(v, w);
    }
}
