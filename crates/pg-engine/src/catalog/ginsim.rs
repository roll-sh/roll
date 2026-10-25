//! GIN indexes' sizes (access/gin): the pages a build lays out (ginbuild: the entries
//! accumulated, sorted, and inserted in order; each key's heap TIDs as a compressed
//! posting list in its entry tuple, or a posting tree when that doesn't fit), and the
//! fast-update pending list rows inserted later go to (ginHeapTupleFastInsert).

use super::heapsim::{BLCKSZ, maxalign};

const PAGE_HEADER: usize = 24;
/// GinPageOpaqueData.
const OPAQUE: usize = 8;
const ITEM_ID: usize = 4;
/// GinMaxItemSize.
const MAX_ITEM_SIZE: usize = 2712;
/// GinDataPageMaxDataSize.
const DATA_PAGE_MAX: usize = BLCKSZ - PAGE_HEADER - 8 - OPAQUE;
/// GinListPageSize and GIN_PAGE_FREESIZE.
const LIST_PAGE_SIZE: usize = BLCKSZ - PAGE_HEADER - OPAQUE;
/// The posting list segment sizes (gindatapage.c).
const SEG_MAX: usize = 384;
const SEG_TARGET: usize = 256;
const SEG_MIN: usize = 128;
/// sizeof(PostingItem): a posting tree's downlink.
const POSTING_ITEM: usize = 10;

fn shortalign(n: usize) -> usize {
    (n + 1) & !1
}

/// A key of the index: its null category (0 for a normal key), a byte string that sorts
/// as the operator class compares keys, and its datum's size in a tuple (from offset 8).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GinKey {
    pub category: u8,
    pub sort: Vec<u8>,
    pub datum: usize,
}

impl GinKey {
    /// index_form_tuple's size for the key alone.
    fn base(&self) -> usize {
        if self.category == 0 {
            maxalign(8 + self.datum)
        } else {
            16
        }
    }

    /// GinFormTuple: the key, then `data` bytes of posting list.
    fn tuple(&self, data: usize) -> usize {
        let mut size = self.base();
        if self.category != 0 {
            size = size.max(16 + 1);
        }
        maxalign(shortalign(size) + data)
    }

    /// GinFormInteriorTuple: the key without its posting list.
    fn interior(&self) -> usize {
        maxalign(self.tuple(0))
    }
}

/// A heap TID as the posting lists encode it.
pub fn tid_code(block: u32, offset: u16) -> u64 {
    ((block as u64) << 11) | offset as u64
}

fn varbyte_len(mut v: u64) -> usize {
    let mut n = 1;
    while v > 0x7F {
        v >>= 7;
        n += 1;
    }
    n
}

/// ginCompressPostingList: (bytes of the GinPostingList, items it holds) for the items
/// fitting `maxsize` bytes.
fn compress(items: &[u64], maxsize: usize) -> (usize, usize) {
    let maxbytes = (maxsize & !1) - 8;
    let mut nbytes = 0;
    let mut n = 1;
    while n < items.len() {
        let len = varbyte_len(items[n] - items[n - 1]);
        if nbytes + len > maxbytes {
            break;
        }
        nbytes += len;
        n += 1;
    }
    (8 + shortalign(nbytes), n)
}

/// A posting tree segment: its items and encoded size (None: to be compressed).
struct Seg {
    items: Vec<u64>,
    size: Option<usize>,
}

/// createPostingTree with ginInsertItemPointers in build mode: the pages the tree takes.
fn posting_tree_pages(items: &[u64]) -> u32 {
    // The root leaf: segments of up to SEG_MAX bytes while they fit.
    let mut segs: Vec<Seg> = Vec::new();
    let mut used = 0;
    let mut i = 0;
    while i < items.len() {
        let (size, n) = compress(&items[i..], SEG_MAX);
        if used + size > DATA_PAGE_MAX {
            break;
        }
        segs.push(Seg {
            items: items[i..i + n].to_vec(),
            size: Some(size),
        });
        used += size;
        i += n;
    }
    let mut pages = 1u32;
    let mut root_split = false;
    let mut leaf = segs;
    while i < items.len() {
        // dataBeginPlaceToPageLeaf, appending to the rightmost leaf.
        let free = DATA_PAGE_MAX - leaf.iter().map(|s| s.size.unwrap_or(0)).sum::<usize>();
        let maxitems = (items.len() - i).min(free + DATA_PAGE_MAX);
        let batch = &items[i..i + maxitems];
        // addItemsToLeaf.
        match leaf.last_mut() {
            Some(last) if last.size.is_some_and(|s| s >= SEG_TARGET) => {
                leaf.push(Seg {
                    items: batch.to_vec(),
                    size: None,
                });
            }
            Some(last) => {
                last.items.extend_from_slice(batch);
                last.size = None;
            }
            None => leaf.push(Seg {
                items: batch.to_vec(),
                size: None,
            }),
        }
        // leafRepackItems.
        let mut out: Vec<Seg> = Vec::new();
        let mut pending: std::collections::VecDeque<Seg> = leaf.into();
        let mut pgused = 0;
        let mut split_at: Option<usize> = None;
        let mut placed_all = true;
        let mut dropped: usize = 0;
        while let Some(mut seg) = pending.pop_front() {
            if seg.size.is_none() {
                let npacked = if seg.items.len() > SEG_MAX {
                    0
                } else {
                    compress(&seg.items, SEG_MAX).1
                };
                if npacked != seg.items.len() {
                    let (size, n) = compress(&seg.items, SEG_TARGET);
                    let rest = seg.items.split_off(n);
                    seg.size = Some(size);
                    pending.push_front(Seg {
                        items: rest,
                        size: None,
                    });
                } else {
                    seg.size = Some(compress(&seg.items, SEG_MAX).0);
                }
            }
            let size = seg.size.unwrap();
            if size < SEG_MIN && !pending.is_empty() {
                let mut next = pending.pop_front().unwrap();
                let mut merged = seg.items;
                merged.extend(next.items);
                next.items = merged;
                next.size = None;
                pending.push_front(next);
                continue;
            }
            if pgused + size > DATA_PAGE_MAX {
                if split_at.is_none() {
                    split_at = Some(out.len());
                    pgused = 0;
                } else {
                    dropped = seg.items.len() + pending.iter().map(|s| s.items.len()).sum::<usize>();
                    placed_all = false;
                    break;
                }
            }
            pgused += size;
            out.push(seg);
        }
        let placed = maxitems - if placed_all { 0 } else { dropped.min(maxitems) };
        i += placed.max(1);
        match split_at {
            Some(at) => {
                let right = out.split_off(at);
                pages += if root_split { 1 } else { 2 };
                root_split = true;
                leaf = right;
            }
            None => leaf = out,
        }
    }
    let _ = POSTING_ITEM;
    pages
}

/// A leaf entry's TIDs: inline in its tuple, or in a posting tree of so many pages.
#[derive(Clone)]
enum Post {
    List(Vec<u64>),
    Tree(Vec<u64>, u32),
}

/// An entry tree item: its key and size on its page; a leaf's TIDs, an internal one's
/// child page.
#[derive(Clone)]
struct Item {
    key: GinKey,
    size: usize,
    child: usize,
    post: Option<Post>,
}

struct Node {
    leaf: bool,
    items: Vec<Item>,
}

/// A GIN index as its pages hold it: the entry tree (with posting trees), the pending
/// list, and the file's pages (some freed for reuse).
pub struct GinIndex {
    nodes: Vec<Node>,
    root: usize,
    /// Pages in the file (the metapage too), and those free for reuse.
    pages: u32,
    free: u32,
    /// The pending list: its pages, the tail's free space, the rows in it (keys, TID).
    pending_pages: u32,
    tail_free: Option<usize>,
    pending: Vec<(Vec<GinKey>, u64)>,
}

impl Default for GinIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl GinIndex {
    /// An empty index: the metapage and the entry tree's root leaf.
    pub fn new() -> GinIndex {
        GinIndex {
            nodes: vec![Node {
                leaf: true,
                items: Vec::new(),
            }],
            root: 0,
            pages: 2,
            free: 0,
            pending_pages: 0,
            tail_free: None,
            pending: Vec::new(),
        }
    }

    /// What SSI names for a key (page ids standing for the pages, consistent for the same
    /// index): the entry tree leaf it is (or would be inserted) on, and its posting tree
    /// when its TIDs are in one (the tree's root, which insertions into it check).
    pub fn key_pages(&self, key: &GinKey) -> (u32, Option<u32>) {
        let mut cur = self.root;
        while !self.nodes[cur].leaf {
            let n = &self.nodes[cur];
            let idx = n
                .items
                .iter()
                .position(|it| it.key >= *key)
                .unwrap_or(n.items.len().saturating_sub(1));
            match n.items.get(idx) {
                Some(it) => cur = it.child,
                None => break,
            }
        }
        let tree = self.nodes[cur].items.iter().find(|it| it.key == *key).and_then(|it| match it.post {
            Some(Post::Tree(..)) => {
                // (A posting tree's identity: its key's.)
                let h = crate::catalog::hashidx::hash_bytes(&key.sort);
                Some(0x4000_0000 | (h & 0x3fff_ffff))
            }
            _ => None,
        });
        (cur as u32 + 1, tree)
    }

    /// Whether rows wait in the pending list.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The main fork's size in pages.
    pub fn nblocks(&self) -> u64 {
        self.pages as u64
    }

    /// The pages of the pending list.
    pub fn pending_pages(&self) -> u32 {
        self.pending_pages
    }

    /// GinNewBuffer: a free page if there is one, else a new one.
    fn alloc(&mut self) {
        if self.free > 0 {
            self.free -= 1;
        } else {
            self.pages += 1;
        }
    }

    fn alloc_n(&mut self, n: u32) {
        for _ in 0..n {
            self.alloc();
        }
    }

    /// A leaf item for `key` with TIDs `items` (buildFreshLeafTuple / a posting list grown
    /// past the tuple: a posting tree).
    fn leaf_item(&mut self, key: GinKey, items: Vec<u64>) -> Item {
        let (bytes, n) = compress(&items, MAX_ITEM_SIZE);
        let size = key.tuple(bytes);
        if n == items.len() && size <= MAX_ITEM_SIZE {
            return Item {
                size,
                key,
                child: 0,
                post: Some(Post::List(items)),
            };
        }
        let pages = posting_tree_pages(&items);
        self.alloc_n(pages);
        Item {
            size: key.tuple(0),
            key,
            child: 0,
            post: Some(Post::Tree(items, pages)),
        }
    }

    /// ginEntryInsert: a key's TIDs into the entry tree.
    pub fn insert(&mut self, key: GinKey, tids: Vec<u64>) {
        // ginFindLeafPage: the first downlink whose key is not below the key's (the
        // rightmost one of a rightmost page covering everything after).
        let mut path: Vec<(usize, usize)> = Vec::new();
        let mut cur = self.root;
        while !self.nodes[cur].leaf {
            let n = &self.nodes[cur];
            let idx = n
                .items
                .iter()
                .position(|it| it.key >= key)
                .unwrap_or(n.items.len() - 1);
            path.push((cur, idx));
            cur = n.items[idx].child;
        }
        let pos = self.nodes[cur].items.partition_point(|it| it.key < key);
        let found = self.nodes[cur]
            .items
            .get(pos)
            .is_some_and(|it| it.key == key);
        if found {
            let old = self.nodes[cur].items[pos].clone();
            let item = match old.post {
                Some(Post::List(mut list)) => {
                    list.extend(tids);
                    list.sort_unstable();
                    list.dedup();
                    self.leaf_item(key, list)
                }
                Some(Post::Tree(mut list, pages)) => {
                    list.extend(tids);
                    list.sort_unstable();
                    list.dedup();
                    let now = posting_tree_pages(&list).max(pages);
                    self.alloc_n(now - pages);
                    Item {
                        post: Some(Post::Tree(list, now)),
                        ..old
                    }
                }
                None => old,
            };
            self.place(cur, path, pos, item, true);
        } else {
            let item = self.leaf_item(key, tids);
            self.place(cur, path, pos, item, false);
        }
    }

    /// A page's tuples, with a new item at `pos` (`replace`: in place of the one there).
    fn place(&mut self, page: usize, mut path: Vec<(usize, usize)>, pos: usize, item: Item, replace: bool) {
        let fits = {
            let items = &self.nodes[page].items;
            let mut used: usize = items.iter().map(|t| t.size + ITEM_ID).sum();
            if replace {
                used -= items[pos].size + ITEM_ID;
            }
            let free = (BLCKSZ - PAGE_HEADER - OPAQUE).saturating_sub(used + ITEM_ID);
            free >= item.size + ITEM_ID
        };
        let items = &mut self.nodes[page].items;
        if replace {
            items[pos] = item;
        } else {
            items.insert(pos, item);
        }
        if fits {
            return;
        }
        // entrySplitPage: halves by size.
        let all = std::mem::take(&mut self.nodes[page].items);
        let total: usize = all.iter().map(|t| t.size + ITEM_ID).sum();
        let mut lsize = 0;
        let mut split = all.len();
        for (i, t) in all.iter().enumerate() {
            if lsize > total / 2 {
                split = i;
                break;
            }
            lsize += t.size + ITEM_ID;
        }
        let mut left = all;
        let right = left.split_off(split);
        let leaf = self.nodes[page].leaf;
        let down = |it: &Item| it.key.interior();
        let lkey = left.last().unwrap().key.clone();
        let lsz = down(left.last().unwrap());
        match path.pop() {
            None => {
                // The root splits: two new pages under it.
                self.alloc_n(2);
                let (l, r) = (self.nodes.len(), self.nodes.len() + 1);
                let rkey = right.last().unwrap().key.clone();
                let rsz = down(right.last().unwrap());
                self.nodes.push(Node { leaf, items: left });
                self.nodes.push(Node { leaf, items: right });
                self.nodes[page].leaf = false;
                self.nodes[page].items = vec![
                    Item {
                        key: lkey,
                        size: lsz,
                        child: l,
                        post: None,
                    },
                    Item {
                        key: rkey,
                        size: rsz,
                        child: r,
                        post: None,
                    },
                ];
            }
            Some((parent, idx)) => {
                self.alloc();
                let r = self.nodes.len();
                self.nodes[page].items = left;
                self.nodes.push(Node { leaf, items: right });
                // The parent's downlink now leads to the right page; the left page's goes in
                // before it.
                self.nodes[parent].items[idx].child = r;
                let item = Item {
                    key: lkey,
                    size: lsz,
                    child: page,
                    post: None,
                };
                self.place(parent, path, idx, item, false);
            }
        }
    }

    /// ginHeapTupleFastInsert: a row's keys into the pending list.
    pub fn insert_pending(&mut self, keys: Vec<GinKey>, tid: u64) {
        if keys.is_empty() {
            return;
        }
        let sizes: Vec<usize> = keys.iter().map(|k| k.tuple(0)).collect();
        let sumsize: usize = sizes.iter().sum();
        let need = sumsize + sizes.len() * ITEM_ID;
        match self.tail_free {
            Some(free) if need <= LIST_PAGE_SIZE && need <= free => {
                self.tail_free = Some(free - need);
            }
            _ => {
                // makeSublist.
                let mut used = 0;
                self.alloc();
                self.pending_pages += 1;
                for s in &sizes {
                    let t = maxalign(*s) + ITEM_ID;
                    if used + t > LIST_PAGE_SIZE {
                        self.alloc();
                        self.pending_pages += 1;
                        used = 0;
                    }
                    used += t;
                }
                self.tail_free = Some(LIST_PAGE_SIZE - used);
            }
        }
        self.pending.push((keys, tid));
    }

    /// ginInsertCleanup (as VACUUM runs it): the pending rows' entries go into the tree, a
    /// key at a time in order; the pending pages are freed for reuse.
    pub fn cleanup(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut acc: std::collections::BTreeMap<GinKey, Vec<u64>> = Default::default();
        for (keys, tid) in std::mem::take(&mut self.pending) {
            for k in keys {
                acc.entry(k).or_default().push(tid);
            }
        }
        for (k, mut tids) in acc {
            tids.sort_unstable();
            self.insert(k, tids);
        }
        self.free += self.pending_pages;
        self.pending_pages = 0;
        self.tail_free = None;
    }
}

/// The main-fork pages of a GIN index built over `built` (key, sorted TIDs) entries, then
/// `pending`: per row inserted after the build, its keys (the fast-update pending list).
pub fn index_pages(built: &[(GinKey, Vec<u64>)], pending: &[Vec<GinKey>]) -> u64 {
    let mut g = GinIndex::new();
    for (k, items) in built {
        g.insert(k.clone(), items.clone());
    }
    for (i, keys) in pending.iter().enumerate() {
        g.insert_pending(keys.clone(), i as u64);
    }
    g.nblocks()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posting_list_sizes() {
        let items: Vec<u64> = (1..=10).map(|i| tid_code(0, i)).collect();
        assert_eq!(compress(&items, MAX_ITEM_SIZE), (18, 10));
    }
}
