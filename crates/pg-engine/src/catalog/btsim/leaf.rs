//! A leaf page's items, packed in one buffer (`BPage::items`). A model holds one item per
//! index entry, so each takes its bytes and nothing else: a flags byte (LP_DEAD), its heap
//! TIDs' count (u16) and TIDs (6 bytes each), then its key's encoding, which gives its own
//! length; and the item's place in the buffer (a u32, in index order). An item rewritten
//! or removed leaves its old bytes behind until they outweigh the live ones.
//! The model encoding can exceed 64 KiB on a legal page with many NULL attributes.
//! Readers get `ItemRef`s; the passes that rework a page (deduplication, deletion, splits)
//! take the items out as owned `Item`s and put them back (`take`, `put`).
//! A page whose items each have one TID and a key of one length (a unique index's, most of a
//! primary key's pages) is kept plain: each item its TID and key, back to back, no flags,
//! count or place (`Leaf::stride`), its LP_DEAD items listed apart; an item of another shape
//! lays it out again first.

use super::{Item, KeyBytes, Tid, maxalign};

/// A packed item, borrowed.
#[derive(Clone, Copy)]
pub(super) struct ItemRef<'a> {
    pub key: &'a KeyBytes,
    pub tids: Tids<'a>,
    pub dead: bool,
}

impl ItemRef<'_> {
    /// The tuple's size (`Item::size`).
    pub fn size(&self) -> usize {
        if self.tids.len() > 1 {
            maxalign(self.key.size() + 6 * self.tids.len())
        } else {
            self.key.size()
        }
    }

    pub fn to_item(self) -> Item {
        let mut it = Item::posting(self.key.to_key(), self.tids.to_vec());
        it.dead = self.dead;
        it
    }
}

/// An item's heap TIDs, packed (ascending).
#[derive(Clone, Copy)]
pub(super) struct Tids<'a>(&'a [u8]);

impl<'a> Tids<'a> {
    pub fn len(&self) -> usize {
        self.0.len() / 6
    }

    pub fn get(&self, i: usize) -> Tid {
        let b = &self.0[6 * i..6 * i + 6];
        (u32::from_le_bytes([b[0], b[1], b[2], b[3]]), u16::from_le_bytes([b[4], b[5]]))
    }

    pub fn first(&self) -> Tid {
        self.get(0)
    }

    pub fn last(&self) -> Tid {
        self.get(self.len() - 1)
    }

    pub fn iter(self) -> impl Iterator<Item = Tid> + 'a {
        self.0.as_chunks::<6>().0.iter().map(|b| (u32::from_le_bytes([b[0], b[1], b[2], b[3]]), u16::from_le_bytes([b[4], b[5]])))
    }

    pub fn to_vec(self) -> Vec<Tid> {
        self.iter().collect()
    }
}

const HEAD: usize = 3;

#[derive(Clone, Debug, Default)]
pub(super) struct Leaf {
    buf: Vec<u8>,
    at: Vec<u32>,
    /// Bytes of `buf` no item holds.
    garbage: usize,
    /// Items not LP_DEAD: a page of none is passed whole by a scan's marking
    /// (`Btree::kill`), without stepping over its items.
    live: u32,
    /// A plain page's bytes per item (6 of TID, then the key), its LP_DEAD items' numbers
    /// in `at` (ascending), `garbage` unused; 0 for a page laid out item by item.
    stride: u32,
}

impl Leaf {
    pub fn len(&self) -> usize {
        match self.stride {
            0 => self.at.len(),
            n => self.buf.len() / n as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Plain if it can be (`stride`).
    pub(super) fn plain(&mut self) {
        if self.stride != 0 || self.at.is_empty() {
            return;
        }
        let first = self.get(0);
        let n = 6 + first.key.as_bytes().len();
        if n > u16::MAX as usize || self.iter().any(|it| it.tids.len() != 1 || 6 + it.key.as_bytes().len() != n) {
            return;
        }
        let mut buf = Vec::with_capacity(n * self.len());
        let mut dead = Vec::new();
        for (i, it) in self.iter().enumerate() {
            buf.extend_from_slice(it.tids.0);
            buf.extend_from_slice(it.key.as_bytes());
            if it.dead {
                dead.push(i as u32);
            }
        }
        *self = Leaf { buf, at: dead, garbage: 0, live: self.live, stride: n as u32 };
    }

    /// Laid out item by item again, for a change.
    pub(super) fn unplain(&mut self) {
        let n = self.stride as usize;
        if n == 0 {
            return;
        }
        let old = std::mem::take(&mut self.buf);
        let dead = std::mem::take(&mut self.at);
        self.stride = 0;
        self.buf = Vec::with_capacity(old.len() + old.len() / n * HEAD + 64);
        self.at = Vec::with_capacity(old.len() / n);
        for (i, item) in old.chunks_exact(n).enumerate() {
            self.at.push(self.buf.len() as u32);
            self.buf.push(dead.binary_search(&(i as u32)).is_ok() as u8);
            self.buf.extend_from_slice(&1u16.to_le_bytes());
            self.buf.extend_from_slice(item);
        }
    }

    /// Items not LP_DEAD.
    pub fn live(&self) -> usize {
        self.live as usize
    }

    fn span(&self, i: usize) -> (usize, usize) {
        let a = self.at[i] as usize;
        let n = u16::from_le_bytes([self.buf[a + 1], self.buf[a + 2]]) as usize;
        let k = a + HEAD + 6 * n;
        (a, k + KeyBytes::len_of(&self.buf[k..]))
    }

    pub fn get(&self, i: usize) -> ItemRef<'_> {
        if self.stride != 0 {
            let b = &self.buf[i * self.stride as usize..(i + 1) * self.stride as usize];
            let dead = !self.at.is_empty() && self.at.binary_search(&(i as u32)).is_ok();
            return ItemRef { key: KeyBytes::of(&b[6..]), tids: Tids(&b[..6]), dead };
        }
        let a = self.at[i] as usize;
        let b = &self.buf[a..];
        let n = u16::from_le_bytes([b[1], b[2]]) as usize;
        let k = &b[HEAD + 6 * n..];
        ItemRef {
            key: KeyBytes::of(&k[..KeyBytes::len_of(k)]),
            tids: Tids(&b[HEAD..HEAD + 6 * n]),
            dead: b[0] & 1 != 0,
        }
    }

    pub fn last(&self) -> Option<ItemRef<'_>> {
        self.len().checked_sub(1).map(|i| self.get(i))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = ItemRef<'_>> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    /// The first item for which `f` is false (`slice::partition_point`).
    pub fn partition_point(&self, mut f: impl FnMut(ItemRef) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if f(self.get(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn set_dead(&mut self, i: usize) {
        if self.stride != 0 {
            if let Err(at) = self.at.binary_search(&(i as u32)) {
                self.at.insert(at, i as u32);
                self.live -= 1;
            }
            return;
        }
        let a = self.at[i] as usize;
        if self.buf[a] & 1 == 0 {
            self.live -= 1;
        }
        self.buf[a] |= 1;
    }

    /// Append an item's bytes, compacting when garbage outweighs the live bytes.
    fn encode(&mut self, dead: bool, tids: &[Tid], k: &[u8]) -> u32 {
        self.unplain();
        let need = HEAD + 6 * tids.len() + k.len();
        if self.garbage > self.buf.len() / 2 {
            self.compact();
        }
        let a = self.buf.len();
        assert!(a + need <= u32::MAX as usize, "leaf page bytes");
        if self.buf.capacity() < a + need {
            // (Grow by an eighth, not double: a model holds a buffer per leaf page.)
            self.buf.reserve_exact(need.max(a / 8).max(64));
        }
        self.buf.push(dead as u8);
        self.buf.extend_from_slice(&(tids.len() as u16).to_le_bytes());
        for &(b, o) in tids {
            self.buf.extend_from_slice(&b.to_le_bytes());
            self.buf.extend_from_slice(&o.to_le_bytes());
        }
        self.buf.extend_from_slice(k);
        self.live += !dead as u32;
        a as u32
    }

    /// Item `i` of a plain page is now one of these parts, if it can be without laying the
    /// page out again (one TID, not LP_DEAD, a key of the page's length): an insertion into
    /// a primary key's page (a non-HOT update's new entry) keeps it plain.
    fn insert_plain(&mut self, i: usize, dead: bool, tids: &[Tid], k: &[u8]) -> bool {
        let n = self.stride as usize;
        let ([(b, o)], false) = (tids, dead) else { return false };
        if n == 0 || 6 + k.len() != n {
            return false;
        }
        let len = self.buf.len();
        if self.buf.capacity() < len + n {
            // (As `encode`'s: an eighth more.)
            self.buf.reserve_exact(n.max(len / 8).max(64));
        }
        let mut item = [0u8; 6];
        item[..4].copy_from_slice(&b.to_le_bytes());
        item[4..].copy_from_slice(&o.to_le_bytes());
        self.buf.splice(i * n..i * n, item.iter().chain(k).copied());
        for d in self.at.iter_mut().rev().take_while(|d| **d as usize >= i) {
            *d += 1;
        }
        self.live += 1;
        true
    }

    /// Append an item given by its parts (`k`: a well-formed key encoding).
    pub fn push_parts(&mut self, dead: bool, tids: &[Tid], k: &[u8]) {
        if self.insert_plain(self.len(), dead, tids, k) {
            return;
        }
        let a = self.encode(dead, tids, k);
        self.at.push(a);
    }

    pub fn insert(&mut self, i: usize, it: &Item) {
        if self.insert_plain(i, it.dead, &it.tids, it.key.as_bytes()) {
            return;
        }
        let a = self.encode(it.dead, &it.tids, it.key.as_bytes());
        self.at.insert(i, a);
    }

    pub fn push(&mut self, it: &Item) {
        if self.insert_plain(self.len(), it.dead, &it.tids, it.key.as_bytes()) {
            return;
        }
        let a = self.encode(it.dead, &it.tids, it.key.as_bytes());
        self.at.push(a);
    }

    pub fn pop(&mut self) -> Option<Item> {
        if self.stride != 0 {
            let it = self.last()?.to_item();
            self.buf.truncate(self.buf.len() - self.stride as usize);
            if it.dead {
                self.at.pop();
            } else {
                self.live -= 1;
            }
            return Some(it);
        }
        self.unplain();
        let i = self.len().checked_sub(1)?;
        let it = self.get(i).to_item();
        let (a, b) = self.span(i);
        self.at.pop();
        self.live -= !it.dead as u32;
        self.garbage += b - a;
        Some(it)
    }

    /// Every item, owned; the page is left empty.
    pub fn take(&mut self) -> Vec<Item> {
        let out = self.iter().map(ItemRef::to_item).collect();
        *self = Leaf::default();
        out
    }

    /// The page's items are `items` (in index order).
    pub fn put(&mut self, items: &[Item]) {
        let bytes: usize = items.iter().map(|it| HEAD + 6 * it.tids.len() + it.key.as_bytes().len()).sum();
        *self = Leaf {
            buf: Vec::with_capacity(bytes),
            at: Vec::with_capacity(items.len()),
            garbage: 0,
            live: 0,
            stride: 0,
        };
        for it in items {
            self.push(it);
        }
        self.plain();
    }

    /// VACUUM's bulk delete on this page (_bt_delitems_vacuum): the TIDs `keep` rejects go,
    /// an item with none left goes, a posting list that loses some is rewritten (its LP_DEAD
    /// mark cleared: PageIndexTupleOverwrite), other items stay as they are. Packed in place,
    /// no item unpacked. Returns (TIDs removed, TIDs left, the items' bytes now).
    pub fn retain_tids(&mut self, keep: impl Fn(Tid) -> bool) -> (usize, usize, usize) {
        let (mut removed, mut left, mut used, mut live) = (0, 0, 0, 0);
        let mut buf = Vec::with_capacity(self.buf.len() - self.garbage);
        let mut at = Vec::with_capacity(self.at.len());
        let mut tids = Vec::new();
        for i in 0..self.len() {
            let it = self.get(i);
            tids.clear();
            tids.extend(it.tids.iter().filter(|&t| keep(t)));
            removed += it.tids.len() - tids.len();
            if tids.is_empty() {
                continue;
            }
            left += tids.len();
            let dead = it.dead && tids.len() == it.tids.len();
            live += !dead as u32;
            used += if tids.len() > 1 { maxalign(it.key.size() + 6 * tids.len()) } else { it.key.size() };
            at.push(buf.len() as u32);
            buf.push(dead as u8);
            buf.extend_from_slice(&(tids.len() as u16).to_le_bytes());
            for &(b, o) in &tids {
                buf.extend_from_slice(&b.to_le_bytes());
                buf.extend_from_slice(&o.to_le_bytes());
            }
            buf.extend_from_slice(it.key.as_bytes());
        }
        *self = Leaf { buf, at, garbage: 0, live, stride: 0 };
        self.plain();
        (removed, left, used)
    }

    /// The buffer without the bytes no item holds.
    fn compact(&mut self) {
        if self.stride != 0 {
            return;
        }
        let mut buf = Vec::with_capacity(self.buf.len() - self.garbage + 64);
        for i in 0..self.len() {
            let (a, b) = self.span(i);
            self.at[i] = buf.len() as u32;
            buf.extend_from_slice(&self.buf[a..b]);
        }
        self.buf = buf;
        self.garbage = 0;
    }

    /// The bytes it holds (for memory accounting).
    pub fn bytes(&self) -> usize {
        self.buf.capacity() + size_of::<u32>() * self.at.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Key;
    use super::*;

    fn show(key: &[u8], tids: Vec<Tid>, dead: bool) -> String {
        format!("{key:?} {tids:?} {dead}")
    }

    /// A packed page reads back as the items put in it, through inserts, LP_DEAD marks,
    /// pops (whose bytes stay until compacted), takes and puts, and repeated buffer rebuilds.
    #[test]
    fn items_read_back() {
        let mut l = Leaf::default();
        let mut want: Vec<Item> = Vec::new();
        let mut x = 3u64;
        for n in 0..6000u32 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = 1 + (x >> 40) as usize % 40;
            let key = Key::new(&[vec![(x >> 8) as u8; len]], &[8 + len]);
            let tids: Vec<Tid> = (0..1 + (x >> 30) as u16 % 3).map(|i| (n, i)).collect();
            match (x >> 50) % 8 {
                0 | 1 if want.len() > 250 || (!want.is_empty() && (x >> 50).is_multiple_of(8)) => {
                    l.pop();
                    want.pop();
                }
                1 if !want.is_empty() => {
                    let i = (x >> 20) as usize % want.len();
                    l.set_dead(i);
                    want[i].dead = true;
                }
                2 => {
                    let items = l.take();
                    assert_eq!(items.len(), want.len());
                    l.put(&items);
                }
                3 if !want.is_empty() => {
                    // (One TID each, none LP_DEAD, keys of one length: put back plain.)
                    for (i, w) in want.iter_mut().enumerate() {
                        *w = Item::posting(Key::new(&[vec![7; 9]], &[17]), vec![(n, i as u16)]);
                    }
                    l.put(&want);
                    assert!(l.stride != 0 && l.at.is_empty());
                }
                4 if l.stride != 0 => {
                    // (LP_DEAD marks keep it plain too.)
                    if !want.is_empty() && x & 2 == 0 {
                        let d = (x >> 24) as usize % want.len();
                        l.set_dead(d);
                        want[d].dead = true;
                    }
                    // (Items of the plain page's shape go in, and pops come out, plain.)
                    let i = (x >> 20) as usize % (want.len() + 1);
                    let it = Item::posting(Key::new(&[vec![(x >> 8) as u8; 9]], &[17]), vec![(n, 9)]);
                    l.insert(i, &it);
                    want.insert(i, it);
                    if x & 1 == 0 {
                        l.pop();
                        want.pop();
                    }
                    assert!(l.stride != 0);
                }
                _ => {
                    let i = (x >> 20) as usize % (want.len() + 1);
                    let it = Item::posting(key, tids);
                    l.insert(i, &it);
                    want.insert(i, it);
                }
            }
            assert_eq!(l.len(), want.len());
            assert_eq!(l.live(), want.iter().filter(|w| !w.dead).count());
            for (i, w) in want.iter().enumerate() {
                let it = l.get(i);
                assert_eq!(show(it.key.as_bytes(), it.tids.to_vec(), it.dead), show(w.key.as_bytes(), w.tids.to_vec(), w.dead));
            }
        }
    }
}
