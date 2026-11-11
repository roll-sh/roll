//! The index as pgvector lays it out on disk: a meta page (block 0), then pages holding
//! element tuples (the vector, its heap tids, its level) and neighbor tuples (index tids of
//! each layer's neighbors), placed as `hnswbuild.c` and `hnswinsert.c` place them. The page
//! model is what decides the relation's size, where a new element lands, which deleted
//! element's space it reuses, and the order VACUUM walks elements in.
//!
//! An element and its neighbor tuple are one slot here (`Elem`): a tid on disk names an
//! element tuple, and a reused tuple (HnswFreeOffset) keeps its tid, so a slot number stands
//! for the tid. Heap tids are the rows' data keys.
//!
//! Records (`AmStore`): the meta page, one per element tuple, one per neighbor tuple; the
//! pages' contents follow from where each tuple is.

use super::mem::maxalign;
use crate::Result;
use crate::ext::index_am::AmStore;

/// InvalidBlockNumber.
pub(super) const INVALID_BLOCK: u32 = u32::MAX;
/// No element (an invalid index tid).
pub(super) const NONE: u32 = u32::MAX;
/// HNSW_HEAPTIDS.
pub(super) const HEAPTIDS: usize = 10;

/// BLCKSZ.
const BLCKSZ: usize = 8192;
/// MAXALIGN(SizeOfPageHeaderData).
const PAGE_HDR: usize = 24;
/// MAXALIGN(sizeof(HnswPageOpaqueData)).
const SPECIAL: usize = 8;
/// sizeof(ItemIdData).
const ITEMID: usize = 4;
/// HNSW_MAX_SIZE: the largest tuple a page holds.
pub(super) const MAX_SIZE: usize = BLCKSZ - PAGE_HDR - SPECIAL - ITEMID;
/// offsetof(HnswElementTupleData, data).
const ETUP_HDR: usize = 72;
/// offsetof(HnswNeighborTupleData, indextids).
const NTUP_HDR: usize = 4;
/// sizeof(ItemPointerData).
const TID: usize = 6;

/// HNSW_ELEMENT_TUPLE_SIZE.
pub(super) fn etup_size(varsize: usize) -> usize {
    maxalign(ETUP_HDR + varsize)
}

/// HNSW_NEIGHBOR_TUPLE_SIZE.
pub(super) fn ntup_size(level: u8, m: u16) -> usize {
    maxalign(NTUP_HDR + TID * (level as usize + 2) * m as usize)
}

/// HnswGetMaxLevel.
pub(super) fn max_level(m: u16) -> u8 {
    let per = (BLCKSZ - PAGE_HDR - SPECIAL - NTUP_HDR - ITEMID) / TID / m as usize;
    (per - 2).min(63) as u8
}

/// HnswGetLayerM: 2 * m connections at layer 0.
#[inline(always)]
pub(super) fn layer_m(m: usize, lc: usize) -> usize {
    if lc == 0 { m * 2 } else { m }
}

/// An element tuple and its neighbor tuple.
#[derive(Clone, Debug)]
pub(super) struct Elem {
    pub blkno: u32,
    pub offno: u16,
    pub nblkno: u32,
    pub noffno: u16,
    pub level: u8,
    pub deleted: bool,
    pub version: u8,
    /// Heap tids (ids of `Graph::heap`), the valid ones first.
    pub heaptids: [u32; HEAPTIDS],
    pub nheap: u8,
    /// lp_len of the element tuple and of the neighbor tuple.
    pub esize: u16,
    pub nsize: u16,
    /// The neighbor tuple: its version, and its index tids (`count` of them: layer `level`
    /// first, `m` each, then layer 0's `2 * m`).
    pub nversion: u8,
    pub neighbors: Box<[u32]>,
}

impl Elem {
    pub fn heaptids(&self) -> &[u32] {
        &self.heaptids[..self.nheap as usize]
    }
}

/// A page: its free space bounds and its items (an element tuple's slot, or a neighbor
/// tuple's with `NTUP` set), in offset order.
#[derive(Clone, Debug)]
pub(super) struct Page {
    lower: u16,
    upper: u16,
    pub items: Vec<u32>,
}

/// An item that is a neighbor tuple.
pub(super) const NTUP: u32 = 1 << 31;

impl Page {
    /// HnswInitPage.
    fn new() -> Page {
        Page { lower: PAGE_HDR as u16, upper: (BLCKSZ - SPECIAL) as u16, items: Vec::new() }
    }

    /// PageGetFreeSpace.
    pub fn free(&self) -> usize {
        (self.upper as usize - self.lower as usize).saturating_sub(ITEMID)
    }

    /// PageGetExactFreeSpace.
    fn exact_free(&self) -> usize {
        self.upper as usize - self.lower as usize
    }

    /// PageAddItem at the next offset: the offset.
    fn add(&mut self, item: u32, size: usize) -> u16 {
        self.lower += ITEMID as u16;
        self.upper -= maxalign(size) as u16;
        self.items.push(item);
        self.items.len() as u16
    }

    /// PageIndexTupleOverwrite's change of the space used.
    fn overwrite(&mut self, old: usize, new: usize) {
        self.upper = (self.upper as usize + maxalign(old) - maxalign(new)) as u16;
    }
}

/// The index: meta page, pages, element tuples, and the values and heap tids they hold.
pub(super) struct Graph {
    pub dims: u32,
    pub m: u16,
    pub ef_construction: u16,
    /// The entry point's slot (NONE: no entry point) and its level as the meta page has it.
    pub entry: u32,
    pub entry_level: i16,
    pub insert_page: u32,
    /// Block `b` is `pages[b - 1]`.
    pub pages: Vec<Page>,
    pub elems: Vec<Elem>,
    /// Values by slot.
    vstart: Vec<usize>,
    vlen: Vec<u32>,
    vdata: Vec<u8>,
    /// Heap tids (data keys) by id.
    hoff: Vec<usize>,
    hdata: Vec<u8>,
    /// Records to write: meta, element tuples, neighbor tuples.
    dirty_meta: bool,
    dirty_e: Vec<u32>,
    dirty_n: Vec<u32>,
}

/// Where AddElementOnDisk put an element, and the insert page it leaves (when it changes).
pub(super) struct Placed {
    pub slot: u32,
    pub insert_page: Option<u32>,
}

impl Graph {
    /// CreateMetaPage: an index with no pages yet.
    pub fn new(dims: u32, m: u16, ef_construction: u16) -> Graph {
        Graph {
            dims,
            m,
            ef_construction,
            entry: NONE,
            entry_level: -1,
            insert_page: INVALID_BLOCK,
            pages: Vec::new(),
            elems: Vec::new(),
            vstart: Vec::new(),
            vlen: Vec::new(),
            vdata: Vec::new(),
            hoff: vec![0],
            hdata: Vec::new(),
            dirty_meta: true,
            dirty_e: Vec::new(),
            dirty_n: Vec::new(),
        }
    }

    /// RelationGetNumberOfBlocks.
    pub fn blocks(&self) -> u64 {
        1 + self.pages.len() as u64
    }

    /// The value of element `slot`.
    #[inline(always)]
    pub fn value(&self, slot: u32) -> &[u8] {
        let s = self.vstart[slot as usize];
        &self.vdata[s..s + self.vlen[slot as usize] as usize]
    }

    fn set_value(&mut self, slot: u32, v: &[u8]) {
        let i = slot as usize;
        if i == self.vstart.len() {
            self.vstart.push(self.vdata.len());
            self.vlen.push(v.len() as u32);
            self.vdata.extend_from_slice(v);
        } else if v.len() <= self.vlen[i] as usize {
            let s = self.vstart[i];
            self.vdata[s..s + v.len()].copy_from_slice(v);
            self.vlen[i] = v.len() as u32;
        } else {
            self.vstart[i] = self.vdata.len();
            self.vlen[i] = v.len() as u32;
            self.vdata.extend_from_slice(v);
        }
    }

    /// Heap tid `id`'s data key.
    pub fn heap(&self, id: u32) -> &[u8] {
        &self.hdata[self.hoff[id as usize]..self.hoff[id as usize + 1]]
    }

    /// A new heap tid.
    pub fn add_heap(&mut self, dkey: &[u8]) -> u32 {
        self.hdata.extend_from_slice(dkey);
        self.hoff.push(self.hdata.len());
        (self.hoff.len() - 2) as u32
    }

    /// The element tuple `slot`'s block and offset, as hash_tid reads a tid.
    pub fn tid_key(&self, slot: u32) -> u64 {
        let e = &self.elems[slot as usize];
        super::mem::tid_key(e.blkno, e.offno)
    }

    pub fn mark_e(&mut self, slot: u32) {
        self.dirty_e.push(slot);
    }

    pub fn mark_n(&mut self, slot: u32) {
        self.dirty_n.push(slot);
    }

    /// A new page at the end of the relation (HnswNewBuffer + HnswInitPage): its block.
    fn append_page(&mut self) -> u32 {
        self.pages.push(Page::new());
        self.pages.len() as u32
    }

    fn page(&mut self, blkno: u32) -> &mut Page {
        &mut self.pages[blkno as usize - 1]
    }

    /// The next page in the chain (HnswPageGetOpaque(page)->nextblkno): pages are only ever
    /// added at the end, after the last one.
    pub fn next_page(&self, blkno: u32) -> u32 {
        if (blkno as usize) < self.pages.len() { blkno + 1 } else { INVALID_BLOCK }
    }

    /// A new element (slot) with no place yet.
    fn new_elem(&mut self, level: u8, heaptids: &[u32], value: &[u8], esize: usize) -> u32 {
        let slot = self.push_elem(level, heaptids, esize);
        self.set_value(slot, value);
        slot
    }

    /// `new_elem` without its value.
    fn push_elem(&mut self, level: u8, heaptids: &[u32], esize: usize) -> u32 {
        let slot = self.elems.len() as u32;
        let mut h = [NONE; HEAPTIDS];
        h[..heaptids.len()].copy_from_slice(heaptids);
        let ncount = (level as usize + 2) * self.m as usize;
        self.elems.push(Elem {
            blkno: INVALID_BLOCK,
            offno: 0,
            nblkno: INVALID_BLOCK,
            noffno: 0,
            level,
            deleted: false,
            version: 1,
            heaptids: h,
            nheap: heaptids.len() as u8,
            esize: esize as u16,
            nsize: ntup_size(level, self.m) as u16,
            nversion: 1,
            neighbors: vec![NONE; ncount].into_boxed_slice(),
        });
        slot
    }

    /// The values a build placed (`place_built_at`), taken over whole: a graph with no
    /// values yet holds them where they are, none copied.
    pub fn adopt_values(&mut self, data: Vec<u8>) {
        assert!(self.vdata.is_empty() && self.vstart.is_empty(), "values adopted by a graph that has some");
        self.vdata = data;
    }

    /// Bytes `start..start + len` of the values (`adopt_values`).
    pub fn value_bytes(&self, start: usize, len: usize) -> &[u8] {
        &self.vdata[start..start + len]
    }
}

impl Graph {
    /// One element of CreateGraphPages (in the build's list order): placed after the last
    /// element, on a new page when the tuples don't fit (element and neighbors on one page
    /// when they can be). Its neighbor tuple is filled later (WriteNeighborTuples).
    /// Its value is at `start..start + len` of the values the graph adopted (`adopt_values`).
    pub fn place_built_at(&mut self, level: u8, heaptids: &[u32], (start, len): (usize, usize), varsize: usize) -> Result<u32> {
        if self.pages.is_empty() {
            self.append_page();
        }
        let esize = etup_size(varsize);
        let nsize = ntup_size(level, self.m);
        let combined = esize + nsize + ITEMID;
        if esize > BLCKSZ {
            return Err(crate::error::Error::new("54000", "index tuple too large"));
        }
        let mut blk = self.pages.len() as u32;
        let free = self.page(blk).free();
        if free < esize || (combined <= MAX_SIZE && free < combined) {
            blk = self.append_page();
        }
        let slot = self.push_elem(level, heaptids, esize);
        self.vstart.push(start);
        self.vlen.push(len as u32);
        let offno = self.page(blk).add(slot, esize);
        let mut nblk = blk;
        if self.page(blk).free() < nsize {
            nblk = self.append_page();
        }
        let noffno = self.page(nblk).add(slot | NTUP, nsize);
        let e = &mut self.elems[slot as usize];
        (e.blkno, e.offno, e.nblkno, e.noffno) = (blk, offno, nblk, noffno);
        Ok(slot)
    }

    /// The end of CreateGraphPages: the insert page is the last page, the entry point is
    /// the graph's.
    pub fn finish_built(&mut self, entry: u32) {
        if self.pages.is_empty() {
            self.append_page();
        }
        self.insert_page = self.pages.len() as u32;
        self.set_entry(entry, true);
    }

    /// HnswUpdateMetaPageInfo's entry point update (`always`: HNSW_UPDATE_ENTRY_ALWAYS,
    /// else HNSW_UPDATE_ENTRY_GREATER).
    pub fn set_entry(&mut self, entry: u32, always: bool) {
        if entry == NONE {
            self.entry = NONE;
            self.entry_level = -1;
        } else {
            let level = self.elems[entry as usize].level as i16;
            if level > self.entry_level || always {
                self.entry = entry;
                self.entry_level = level;
            }
        }
        self.dirty_meta = true;
    }

    /// HnswUpdateMetaPage of the insert page.
    pub fn set_insert_page(&mut self, page: u32) {
        if page != INVALID_BLOCK {
            self.insert_page = page;
            self.dirty_meta = true;
        }
    }

    /// HnswFreeOffset: a deleted element on page `blk` whose tuples' space the new ones fit
    /// in (its slot), noting the first page with a deleted element as the new insert page.
    fn free_offset(&self, blk: u32, esize: usize, nsize: usize, new_insert_page: &mut u32) -> Option<u32> {
        let page = &self.pages[blk as usize - 1];
        for &item in &page.items {
            if item & NTUP != 0 {
                continue;
            }
            let e = &self.elems[item as usize];
            if !e.deleted {
                continue;
            }
            if *new_insert_page == INVALID_BLOCK {
                *new_insert_page = blk;
            }
            let page_free = e.esize as usize + page.exact_free();
            let mut npage_free = e.nsize as usize;
            if e.nblkno != blk {
                npage_free += self.pages[e.nblkno as usize - 1].exact_free();
            } else if page_free >= esize {
                npage_free += page_free - esize;
            }
            if page_free >= esize && npage_free >= nsize {
                return Some(item);
            }
        }
        None
    }

    /// AddElementOnDisk: place a new element (its heap tids, value, level) starting at the
    /// insert page: both tuples on a page with room, or a deleted element's space, or new
    /// pages at the end. Its neighbor tuple is the caller's to fill.
    pub fn add_element_on_disk(&mut self, level: u8, heaptid: u32, value: &[u8], varsize: usize) -> Placed {
        let insert_page = self.insert_page;
        let esize = etup_size(varsize);
        let nsize = ntup_size(level, self.m);
        let combined = esize + nsize + ITEMID;
        let min_combined = esize + ntup_size(0, self.m) + ITEMID;
        let mut new_insert_page = INVALID_BLOCK;
        let mut cur = insert_page;
        // (block of the element tuple, of the neighbor tuple, a reused slot)
        let (blk, nblk, reuse) = loop {
            let free = self.pages[cur as usize - 1].free();
            if new_insert_page == INVALID_BLOCK && free >= min_combined {
                new_insert_page = cur;
            }
            if free >= combined {
                break (cur, cur, None);
            }
            if let Some(s) = self.free_offset(cur, esize, nsize, &mut new_insert_page) {
                break (cur, self.elems[s as usize].nblkno, Some(s));
            }
            let next = self.next_page(cur);
            if combined > MAX_SIZE && free >= esize && next == INVALID_BLOCK {
                let n = self.append_page();
                break (cur, n, None);
            }
            if next != INVALID_BLOCK {
                cur = next;
                continue;
            }
            let b = self.append_page();
            let n = if self.pages[b as usize - 1].free() < combined { self.append_page() } else { b };
            break (b, n, None);
        };
        if new_insert_page == INVALID_BLOCK {
            new_insert_page = nblk;
        }
        let slot = match reuse {
            Some(s) => {
                // The deleted element's tuples are overwritten, its version kept.
                let version = self.elems[s as usize].version;
                let (old_e, old_n) = (self.elems[s as usize].esize as usize, self.elems[s as usize].nsize as usize);
                let ncount = (level as usize + 2) * self.m as usize;
                let e = &mut self.elems[s as usize];
                let mut h = [NONE; HEAPTIDS];
                h[0] = heaptid;
                (e.level, e.deleted, e.version, e.heaptids, e.nheap) = (level, false, version, h, 1);
                (e.esize, e.nsize, e.nversion) = (esize as u16, nsize as u16, version);
                e.neighbors = vec![NONE; ncount].into_boxed_slice();
                self.page(blk).overwrite(old_e, esize);
                self.page(nblk).overwrite(old_n, nsize);
                self.set_value(s, value);
                s
            }
            None => {
                let s = self.new_elem(level, &[heaptid], value, esize);
                let offno = self.page(blk).add(s, esize);
                let noffno = self.page(nblk).add(s | NTUP, nsize);
                let e = &mut self.elems[s as usize];
                (e.blkno, e.offno, e.nblkno, e.noffno) = (blk, offno, nblk, noffno);
                s
            }
        };
        self.mark_e(slot);
        self.mark_n(slot);
        let insert_page = (new_insert_page != INVALID_BLOCK && new_insert_page != insert_page).then_some(new_insert_page);
        Placed { slot, insert_page }
    }
}

// ── records ──

const META_KEY: u8 = b'm';
const ELEM_KEY: u8 = b'e';
const NEIGHBOR_KEY: u8 = b'n';

fn key(kind: u8, slot: u32) -> Vec<u8> {
    let mut k = vec![kind];
    k.extend_from_slice(&slot.to_be_bytes());
    k
}

/// A record's fields, read in order.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(crate::error::internal("hnsw: truncated index record"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

impl Graph {
    fn meta_record(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(24);
        b.extend_from_slice(&self.dims.to_le_bytes());
        b.extend_from_slice(&self.m.to_le_bytes());
        b.extend_from_slice(&self.ef_construction.to_le_bytes());
        b.extend_from_slice(&self.entry.to_le_bytes());
        b.extend_from_slice(&self.entry_level.to_le_bytes());
        b.extend_from_slice(&self.insert_page.to_le_bytes());
        b.extend_from_slice(&(self.pages.len() as u32).to_le_bytes());
        b
    }

    fn elem_record(&self, slot: u32) -> Vec<u8> {
        let e = &self.elems[slot as usize];
        let v = self.value(slot);
        let mut b = Vec::with_capacity(32 + v.len());
        b.extend_from_slice(&e.blkno.to_le_bytes());
        b.extend_from_slice(&e.offno.to_le_bytes());
        b.extend_from_slice(&e.nblkno.to_le_bytes());
        b.extend_from_slice(&e.noffno.to_le_bytes());
        b.extend_from_slice(&[e.level, e.deleted as u8, e.version]);
        b.extend_from_slice(&e.esize.to_le_bytes());
        b.extend_from_slice(&e.nsize.to_le_bytes());
        b.push(e.nheap);
        for &h in e.heaptids() {
            let d = self.heap(h);
            b.extend_from_slice(&(d.len() as u16).to_le_bytes());
            b.extend_from_slice(d);
        }
        b.extend_from_slice(&(v.len() as u32).to_le_bytes());
        b.extend_from_slice(v);
        b
    }

    fn neighbor_record(&self, slot: u32) -> Vec<u8> {
        let e = &self.elems[slot as usize];
        let mut b = Vec::with_capacity(3 + 4 * e.neighbors.len());
        b.push(e.nversion);
        b.extend_from_slice(&(e.neighbors.len() as u16).to_le_bytes());
        for &n in e.neighbors.iter() {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b
    }

    /// Write the records changed since the last write (`all`: every record, a build's).
    pub fn flush(&mut self, store: &AmStore, all: bool) -> Result<()> {
        if all {
            self.dirty_e = (0..self.elems.len() as u32).collect();
            self.dirty_n = self.dirty_e.clone();
        } else {
            self.dirty_e.sort_unstable();
            self.dirty_e.dedup();
            self.dirty_n.sort_unstable();
            self.dirty_n.dedup();
        }
        let mut w: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        if self.dirty_meta || all {
            w.push((vec![META_KEY], Some(self.meta_record())));
        }
        let (dirty_e, dirty_n) = (std::mem::take(&mut self.dirty_e), std::mem::take(&mut self.dirty_n));
        // (A build's records go in batches, each made as it is written: the whole index's
        // records are never in memory at once.)
        let records = dirty_e.iter().map(|&s| (ELEM_KEY, s)).chain(dirty_n.iter().map(|&s| (NEIGHBOR_KEY, s)));
        for (kind, s) in records {
            let rec = if kind == ELEM_KEY { self.elem_record(s) } else { self.neighbor_record(s) };
            w.push((key(kind, s), Some(rec)));
            if w.len() == 4096 {
                store.write(std::mem::take(&mut w))?;
            }
        }
        self.dirty_meta = false;
        store.write(w)
    }

    /// The index from its records (None: there are none).
    pub fn load(records: &[(Vec<u8>, bytes::Bytes)]) -> Result<Option<Graph>> {
        let Some((_, meta)) = records.iter().find(|(k, _)| k.as_slice() == [META_KEY]) else {
            return Ok(None);
        };
        let mut r = Reader(meta);
        let mut g = Graph::new(r.u32()?, r.u16()?, r.u16()?);
        g.entry = r.u32()?;
        g.entry_level = r.u16()? as i16;
        g.insert_page = r.u32()?;
        let npages = r.u32()? as usize;
        let mut nb: Vec<Option<(u8, Box<[u32]>)>> = Vec::new();
        for (k, v) in records {
            if k.len() != 5 || (k[0] != ELEM_KEY && k[0] != NEIGHBOR_KEY) {
                continue;
            }
            let slot = u32::from_be_bytes(k[1..5].try_into().unwrap()) as usize;
            let mut r = Reader(v);
            if k[0] == NEIGHBOR_KEY {
                let version = r.u8()?;
                let count = r.u16()? as usize;
                let n: Box<[u32]> = (0..count).map(|_| r.u32()).collect::<Result<_>>()?;
                if nb.len() <= slot {
                    nb.resize(slot + 1, None);
                }
                nb[slot] = Some((version, n));
                continue;
            }
            if slot != g.elems.len() {
                return Err(crate::error::internal("hnsw: missing element record"));
            }
            let (blkno, offno, nblkno, noffno) = (r.u32()?, r.u16()?, r.u32()?, r.u16()?);
            let (level, deleted, version) = (r.u8()?, r.u8()? != 0, r.u8()?);
            let (esize, nsize, nheap) = (r.u16()?, r.u16()?, r.u8()?);
            let mut heaptids = [NONE; HEAPTIDS];
            for h in heaptids.iter_mut().take(nheap as usize) {
                let n = r.u16()? as usize;
                *h = g.add_heap(r.take(n)?);
            }
            let n = r.u32()? as usize;
            let value = r.take(n)?;
            g.set_value(slot as u32, value);
            g.elems.push(Elem { blkno, offno, nblkno, noffno, level, deleted, version, heaptids, nheap, esize, nsize, nversion: version, neighbors: Box::new([]) });
        }
        for (slot, e) in g.elems.iter_mut().enumerate() {
            if let Some(Some((version, n))) = nb.get_mut(slot).map(Option::take) {
                e.nversion = version;
                e.neighbors = n;
            }
        }
        // The pages, from where each tuple is.
        g.pages = vec![Page::new(); npages];
        for (slot, e) in g.elems.iter().enumerate() {
            for (blk, off, item, size) in [(e.blkno, e.offno, slot as u32, e.esize), (e.nblkno, e.noffno, slot as u32 | NTUP, e.nsize)] {
                let p = &mut g.pages[blk as usize - 1];
                if p.items.len() < off as usize {
                    p.items.resize(off as usize, NONE);
                }
                p.items[off as usize - 1] = item;
                p.upper -= maxalign(size as usize) as u16;
            }
        }
        for p in &mut g.pages {
            p.lower += (ITEMID * p.items.len()) as u16;
        }
        g.dirty_meta = false;
        Ok(Some(g))
    }
}
