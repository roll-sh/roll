//! The leaf pages of a btree of a table too big to model (`sizing::is_estimated`), kept
//! as a summary: per leaf its key range, its items' bytes, tuples, heap TIDs and distinct
//! keys, no entry of its own. An index build lays the leaves out as its sorted build does
//! (`relsize::LevelFill`); the table's writes then add their entries where their keys go,
//! a leaf that no longer has room is deduplicated or split as nbtree does it, and VACUUM
//! takes the dead entries out. The planner, pg_relation_size and VACUUM's relpages read its
//! pages: PostgreSQL's index grows with its table's writes (a bulk UPDATE doubles it) and
//! VACUUM doesn't shrink it, where the last measure of a big table's index stayed its size.
//!
//! Within a leaf the keys are taken as spread evenly over its range, so a split's key is
//! interpolated, and an insert's key is the first 16 bytes of its sort key (`key_prefix`).
//! The model proper (`btsim`) holds every entry; this costs a few dozen bytes a leaf.
use crate::catalog::relsize::{self, BT_USABLE, LevelFill};

const ITEM_ID: f64 = 4.0;
/// nbtree's leaf fillfactor (BTREE_DEFAULT_FILLFACTOR): a rightmost split leaves that much
/// of the page on the left.
const LEAF_FILL: f64 = 0.90;

/// A sort key's first 16 bytes as one big-endian number, zero-padded.
pub fn key_prefix(k: &[u8]) -> u128 {
    let mut b = [0u8; 16];
    let n = k.len().min(16);
    b[..n].copy_from_slice(&k[..n]);
    u128::from_be_bytes(b)
}

#[derive(Clone, Debug)]
struct Leaf {
    /// The greatest key in the leaf.
    max: u128,
    /// Bytes of its items, their line pointers included.
    used: f64,
    tuples: f64,
    tids: f64,
    /// Distinct keys (what deduplication merges down to).
    keys: f64,
}

/// One btree's leaves (`btest`'s summary): their low keys (and an order among leaves of one
/// key) in key order, searched by halving, each with its leaf's place in `store` (a split
/// moves 28 bytes a leaf, not the leaves).
#[derive(Clone, Debug)]
pub struct BtEst {
    lows: Vec<(u128, u64)>,
    slots: Vec<u32>,
    store: Vec<Leaf>,
    /// Deduplication (a non-unique index of equal-image types).
    dedup: bool,
    /// A plain tuple's size (MAXALIGNed), and a pivot's (high keys, downlinks).
    base: f64,
    pivot: f64,
    /// Inner pages at the build, and what the leaves' count then gave by `inner_for`.
    inner_built: usize,
    inner_base: usize,
    /// Deleted pages not reused yet (VACUUM's page deletion; splits take them first).
    free: usize,
    /// Pages split off a leaf whose key range had no order left to give them.
    stuck: usize,
    /// The file's blocks (it never shrinks).
    blocks: u32,
}

/// What a build hands the summary for each leaf it lays out.
#[derive(Default)]
pub struct EstBuilder {
    fill: Option<LevelFill>,
    dedup: bool,
    leaves: Vec<(u128, Leaf)>,
    base_sum: f64,
    base_n: f64,
    pivot_sum: f64,
}

impl EstBuilder {
    pub fn new(dedup: bool) -> EstBuilder {
        EstBuilder { fill: Some(LevelFill::leaves()), dedup, ..Default::default() }
    }

    /// The build's next index tuple, in key order: its size, its pivot's, its key and heap
    /// TIDs; `first`: the first tuple of its key.
    pub fn push(&mut self, size: usize, pivot: usize, key: u128, tids: usize, first: bool, base: usize) {
        let fill = self.fill.as_mut().expect("a fill");
        if fill.push(size, pivot) {
            self.pivot_sum += pivot as f64;
            self.leaves.push((key, Leaf { max: key, used: 0.0, tuples: 0.0, tids: 0.0, keys: 0.0 }));
        }
        let used = fill.used() as f64;
        let l = &mut self.leaves.last_mut().expect("a leaf").1;
        l.max = key;
        l.used = used;
        l.tuples += 1.0;
        l.tids += tids as f64;
        if first || l.keys == 0.0 {
            l.keys += 1.0;
        }
        self.base_sum += base as f64;
        self.base_n += 1.0;
    }

    /// The summary, with the build's pages (`relsize::btree_pages`'s count).
    pub fn finish(mut self) -> (i32, BtEst) {
        let fill = self.fill.take().expect("a fill");
        let inner = if fill.firsts.is_empty() { 0 } else { relsize::btree_inner_pages(&fill.firsts) };
        let n = self.leaves.len();
        let base = if self.base_n > 0.0 { self.base_sum / self.base_n } else { 16.0 };
        let pivot = if n > 0 { self.pivot_sum / n as f64 } else { base };
        let (mut lows, mut store) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for (i, (lo, l)) in self.leaves.into_iter().enumerate() {
            lows.push((lo, (i as u64) << 24));
            store.push(l);
        }
        let mut e = BtEst {
            lows,
            slots: (0..n as u32).collect(),
            store,
            dedup: self.dedup,
            base,
            pivot,
            inner_built: inner,
            inner_base: 0,
            free: 0,
            stuck: 0,
            blocks: 0,
        };
        e.inner_base = e.inner_for(n);
        let pages = if n == 0 { 1 } else { 1 + n + inner } as u32;
        e.blocks = pages;
        (pages as i32, e)
    }
}

impl BtEst {
    /// The summary of a modelled btree's leaves (`Btree::leaf_summaries`) with its blocks:
    /// a table that grew too big to model keeps its indexes' shapes.
    pub fn from_model(leaves: Vec<crate::catalog::btsim::LeafSummary>, dedup: bool, blocks: u32) -> BtEst {
        let n = leaves.len();
        let (plain, tuples): (usize, usize) = leaves.iter().fold((0, 0), |a, l| (a.0 + l.plain, a.1 + l.tuples));
        let base = if tuples > 0 { plain as f64 / tuples as f64 } else { 16.0 };
        let (mut lows, mut store) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for (i, l) in leaves.into_iter().enumerate() {
            store.push(Leaf {
                max: key_prefix(&l.last),
                used: l.used as f64,
                tuples: l.tuples as f64,
                tids: l.tids as f64,
                keys: l.keys as f64,
            });
            // (Low keys kept in order: an empty leaf's is its predecessor's.)
            let lo = key_prefix(&l.first).max(lows.last().map_or(0, |p: &(u128, u64)| p.0));
            lows.push((lo, (i as u64) << 24));
        }
        let mut e = BtEst {
            lows,
            slots: (0..n as u32).collect(),
            store,
            dedup,
            base,
            pivot: base,
            inner_built: 0,
            inner_base: 0,
            free: 0,
            stuck: 0,
            blocks,
        };
        e.inner_base = e.inner_for(n);
        // (Its inner and deleted pages as the model has them: the blocks past the leaves.)
        e.inner_built = (blocks as usize).saturating_sub(1 + n);
        e
    }

    /// An empty index (a metapage).
    pub fn empty(dedup: bool, base: usize) -> BtEst {
        EstBuilder { dedup, base_sum: base as f64, base_n: 1.0, ..EstBuilder::new(dedup) }.finish().1
    }

    /// The file's blocks.
    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    /// Index entries (heap TIDs) it holds, dead ones included.
    pub fn entries(&self) -> f64 {
        self.slots.iter().map(|&s| self.store[s as usize].tids).sum()
    }

    /// Inner pages above `leaves` leaves filled as a build fills them.
    fn inner_for(&self, leaves: usize) -> usize {
        let p = self.pivot.round() as usize;
        let firsts = vec![(p, p); leaves];
        if leaves == 0 { 0 } else { relsize::btree_inner_pages(&firsts) }
    }

    fn note_blocks(&mut self) {
        let n = self.lows.len();
        let inner = (self.inner_built + self.inner_for(n)).saturating_sub(self.inner_base);
        let pages = (1 + n.max(1) + inner + self.free + self.stuck) as u32;
        self.blocks = self.blocks.max(pages);
    }

    /// A leaf's room for items: a page less its high key (none on the rightmost).
    fn room(&self, rightmost: bool) -> f64 {
        BT_USABLE as f64 - if rightmost { 0.0 } else { self.pivot + ITEM_ID }
    }

    /// A tuple of the deduplicated size of a leaf's `keys` and `tids` (each key's TIDs in
    /// posting lists as `relsize::posting_split` makes them), line pointers included.
    fn merged(&self, keys: f64, tids: f64) -> f64 {
        if keys <= 0.0 {
            return 0.0;
        }
        let per_key = (tids / keys).max(1.0);
        let base = self.base.round() as usize;
        let per = relsize::posting_per(base).max(1);
        // (A key's TIDs in full posting lists, then the rest in one: a key of `k` TIDs.)
        let of = |k: usize| {
            let (full, rest) = (k / per, k % per);
            let mut one = full as f64 * (relsize::posting_size(base, per) as f64 + ITEM_ID);
            if rest > 0 {
                one += relsize::posting_size(base, rest) as f64 + ITEM_ID;
            }
            one
        };
        // (Between the sizes of the whole numbers of TIDs around the mean: a posting list
        // grows by MAXALIGN steps, not 6 bytes a TID.)
        let k = per_key.floor() as usize;
        let f = per_key - k as f64;
        keys * (of(k) * (1.0 - f) + of(k + 1) * f)
    }

    /// An entry of `size` bytes (MAXALIGNed) under `key` goes in; `dup`: its key is in the
    /// index already (an UPDATE's new version, the indexed columns unchanged).
    pub fn insert(&mut self, key: u128, size: usize, dup: bool) {
        let need = size as f64 + ITEM_ID;
        if self.lows.is_empty() {
            self.lows.push((key, 0));
            self.slots.push(self.store.len() as u32);
            self.store.push(Leaf { max: key, used: 0.0, tuples: 0.0, tids: 0.0, keys: 0.0 });
        }
        // (The last leaf whose low key is at or before the key's: a new entry goes after
        // its key's others, its heap TID being the greatest.)
        let i = self.lows.partition_point(|lo| lo.0 <= key).saturating_sub(1);
        let rightmost = i + 1 == self.lows.len();
        let room = self.room(rightmost);
        let dedup = self.dedup;
        let slot = self.slots[i] as usize;
        let l = &self.store[slot];
        // (A row's key in a leaf of distinct keys is a new one; in a leaf whose keys have
        // two TIDs or more each, one of its keys again: such an index's values repeat.)
        let new_key = if dup {
            0.0
        } else if dedup && l.tids > 0.0 {
            (2.0 * l.keys / l.tids - 1.0).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // PageGetFreeSpace leaves room for one more line pointer.
        if room - l.used - ITEM_ID >= need {
            Self::add(&mut self.store[slot], key, need, new_key);
            return;
        }
        // A full leaf is deduplicated first (_bt_dedup_pass), when that makes room.
        if dedup && l.tids > l.keys {
            let merged = self.merged(l.keys, l.tids);
            let l = &mut self.store[slot];
            if merged < l.used {
                l.used = merged;
                l.tuples = l.keys;
                if room - l.used - ITEM_ID >= need {
                    Self::add(l, key, need, new_key);
                    return;
                }
            }
        }
        self.split(i, rightmost, key, need, new_key);
    }

    fn add(l: &mut Leaf, key: u128, need: f64, new_key: f64) {
        l.used += need;
        l.tuples += 1.0;
        l.tids += 1.0;
        l.keys += new_key;
        l.max = l.max.max(key);
    }

    /// _bt_split of leaf `at` for an entry under `key` (nbtsplitloc): a rightmost leaf
    /// taking a key past its last splits at the fillfactor (the left page keeps 90%), any
    /// other evenly by bytes; the split key is where that share of the leaf's range ends.
    fn split(&mut self, i: usize, rightmost: bool, key: u128, need: f64, new_key: f64) {
        let room = self.room(rightmost);
        let at = self.lows[i];
        let slot = self.slots[i] as usize;
        let mut l = self.store[slot].clone();
        let lo = at.0;
        let total = l.used + need;
        let append = rightmost && key >= l.max;
        let left_share = if append {
            (room * LEAF_FILL / l.used).min(1.0)
        } else {
            (total / 2.0 / l.used).min(1.0)
        };
        let span = l.max.saturating_sub(lo);
        let cut = lo + ((span as f64) * left_share) as u128;
        let mut right = Leaf {
            max: l.max,
            used: l.used * (1.0 - left_share),
            tuples: l.tuples * (1.0 - left_share),
            tids: l.tids * (1.0 - left_share),
            keys: l.keys * (1.0 - left_share),
        };
        l.used -= right.used;
        l.tuples -= right.tuples;
        l.tids -= right.tids;
        l.keys -= right.keys;
        // (The right page's low key: after the left's, before the next leaf's.)
        let next = self.lows.get(i + 1).copied();
        let below_next = |k: (u128, u64)| next.is_none_or(|n| k < n);
        let mut right_at = if cut > lo { (cut, 0) } else { (lo, at.1 + (u64::MAX - at.1) / 2) };
        if !below_next(right_at) {
            // (Between the left's place and the next leaf's, in the order among one key's.)
            let n = next.expect("a next leaf");
            right_at = if n.0 == at.0 { (lo, at.1 + (n.1 - at.1) / 2) } else { (n.0, n.1 / 2) };
        }
        if !(right_at > at && below_next(right_at)) {
            // (No order left between them: the page counted, the leaf kept whole.)
            l.used += right.used;
            l.tuples += right.tuples;
            l.tids += right.tids;
            l.keys += right.keys;
            Self::add(&mut l, key, need, new_key);
            l.used = l.used.min(room);
            self.store[slot] = l;
            self.stuck += 1;
            self.note_blocks();
            return;
        }
        if right_at.0 > lo {
            l.max = right_at.0 - 1;
        }
        let into_right = append || key >= right_at.0 && (right_at.0 > lo || l.tuples <= right.tuples);
        if into_right {
            Self::add(&mut right, key, need, new_key);
        } else {
            Self::add(&mut l, key, need, new_key);
        }
        self.store[slot] = l;
        self.lows.insert(i + 1, right_at);
        self.slots.insert(i + 1, self.store.len() as u32);
        self.store.push(right);
        // (A deleted page is reused before the file grows.)
        self.free = self.free.saturating_sub(1);
        self.note_blocks();
    }

    /// VACUUM found `live` of the entries live: the dead ones go from every leaf alike
    /// (btbulkdelete), and a leaf left empty is deleted (its page free for a later split),
    /// but the rightmost. Returns the file's blocks.
    pub fn vacuum(&mut self, live: f64) -> u32 {
        let total = self.entries();
        if total <= 0.0 {
            return self.blocks;
        }
        let keep = (live / total).clamp(0.0, 1.0);
        if keep < 1.0 {
            let n = self.lows.len();
            let dedup = self.dedup;
            let mut gone = 0;
            let mut at = 0;
            for i in 0..n {
                let l = &mut self.store[self.slots[i] as usize];
                if l.tids * keep < 0.5 && i + 1 < n {
                    gone += 1;
                    continue;
                }
                l.used *= keep;
                l.tuples *= keep;
                l.tids *= keep;
                l.keys = if dedup { (l.keys * keep).max(l.tids.min(1.0)) } else { l.keys * keep };
                (self.lows[at], self.slots[at]) = (self.lows[i], self.slots[i]);
                at += 1;
            }
            self.lows.truncate(at);
            self.slots.truncate(at);
            self.free += gone;
            self.note_blocks();
        }
        self.blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(n: u64, dedup: bool, per_key: u64) -> (i32, BtEst) {
        let mut b = EstBuilder::new(dedup);
        let mut sizes = Vec::new();
        for k in 0..n / per_key {
            let key = (k as u128 + 1) << 88;
            let mut first = true;
            relsize::posting_split(16, if dedup { per_key as usize } else { 1 }, |size, pivot, tids| {
                b.push(size, pivot, key, tids, first, 16);
                sizes.push((size, pivot));
                first = false;
            });
            if !dedup {
                for _ in 1..per_key {
                    b.push(16, 24, key, 1, false, 16);
                    sizes.push((16, 24));
                }
            }
        }
        let (pages, e) = b.finish();
        assert_eq!(pages, relsize::btree_pages(&sizes));
        (pages, e)
    }

    /// A unique int index built over 1M keys, then half its keys given a second entry in
    /// key order (a bulk UPDATE's new versions): about twice the pages, as PostgreSQL's
    /// 2,745 -> 5,486.
    #[test]
    fn a_bulk_update_doubles_a_unique_index() {
        let (pages, mut e) = built(1_000_000, false, 1);
        assert_eq!(pages, 2745);
        for k in (2..=1_000_000u64).step_by(2) {
            e.insert((k as u128) << 88, 16, true);
        }
        let grown = e.blocks();
        assert!((5300..=5700).contains(&grown), "{grown}");
        // VACUUM after a quarter of the rows went: the file keeps its size.
        assert_eq!(e.vacuum(750_000.0), grown);
    }

    /// Keys past the last one fill new leaves to 90%: 200,000 of them add about 548 pages
    /// (PostgreSQL: 2,745 -> 3,293).
    #[test]
    fn appended_keys_fill_leaves_to_the_fillfactor() {
        let (_, mut e) = built(1_000_000, false, 1);
        for k in 1_000_001..=1_200_000u64 {
            e.insert((k as u128) << 88, 16, false);
        }
        let grown = e.blocks() as i64;
        assert!((3270..=3320).contains(&grown), "{grown}");
    }
}
