//! Redis 8's dict for objects that hold their own key (`no_value` dicts of kvobjs): the same
//! buckets, incremental rehashing, SCAN and sampling as [`crate::dict::RawDict`], but a bucket
//! holds an object's pointer directly, and only keys that share a bucket take a 16-byte chain
//! entry. Objects carry no chain link of their own, and two tables can index the same objects
//! (the keyspace and the keys with a TTL).
//!
//! The table only points at objects; whoever owns them frees them ([`crate::keyspace`]).

use foldhash::fast::RandomState;
use std::hash::BuildHasher;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::dict::DictStats;
use crate::util::Rng;

/// An object that holds its key.
///
/// # Safety
/// `key` must read the key of a live object at `p`, unchanged while it is in a table.
pub unsafe trait Keyed {
    /// # Safety
    /// `p` points at a live object.
    unsafe fn key<'a>(p: NonNull<Self>) -> &'a [u8];
}

/// A bucket or chain link: 0 (nothing), an object pointer, or a [`Chain`] pointer with the low
/// bit set.
type Link = usize;

/// A chain entry: an object and the rest of the chain (never empty: the last object of a chain
/// sits in the link directly).
#[repr(C)]
struct Chain {
    node: Link,
    next: Link,
}

#[inline]
fn is_chain(l: Link) -> bool {
    l & 1 != 0
}

#[inline]
fn chain(l: Link) -> *mut Chain {
    (l & !1) as *mut Chain
}

fn new_chain(node: Link, next: Link) -> Link {
    debug_assert!(next != 0);
    Box::into_raw(Box::new(Chain { node, next })) as Link | 1
}

/// # Safety
/// `l` is a chain link no longer referenced.
unsafe fn free_chain(l: Link) {
    // SAFETY: allocated by `new_chain`.
    drop(unsafe { Box::from_raw(chain(l)) });
}

pub struct PtrDict<N: Keyed> {
    t0: Vec<Link>,
    /// Non-empty only while rehashing from `t0` into it.
    t1: Vec<Link>,
    /// Next bucket of `t0` to move while rehashing.
    rehash_idx: usize,
    len: usize,
    hasher: RandomState,
    /// Grows and shrinks on its own (DEBUG DICT-RESIZING 0 turns this off).
    resize: bool,
    _n: PhantomData<*mut N>,
}

// SAFETY: the table holds pointers to objects owned elsewhere; whoever shares it across threads
// shares the objects with it ([`crate::keyspace::KeySpace`] owns both).
unsafe impl<N: Keyed> Send for PtrDict<N> {}
// SAFETY: as above; shared access only reads.
unsafe impl<N: Keyed> Sync for PtrDict<N> {}

const MIN_SIZE: usize = 4;
/// Empty buckets visited per rehash step before giving up (Redis uses 10x the step count).
const EMPTY_VISITS: usize = 10;

impl<N: Keyed> Default for PtrDict<N> {
    fn default() -> Self {
        Self::with_hasher(RandomState::default())
    }
}

/// The objects of the chain at `l`, in order, each with the link that holds its pointer.
struct Walk {
    l: Link,
}

impl Iterator for Walk {
    type Item = Link;
    #[inline]
    fn next(&mut self) -> Option<Link> {
        match self.l {
            0 => None,
            l if is_chain(l) => {
                // SAFETY: a live chain entry of the table being walked.
                let c = unsafe { &*chain(l) };
                self.l = c.next;
                Some(c.node)
            }
            l => {
                self.l = 0;
                Some(l)
            }
        }
    }
}

#[inline]
fn walk(l: Link) -> Walk {
    Walk { l }
}

/// Start loading the cache line at `p` (a hint: no fault, no effect on results).
#[inline(always)]
pub fn prefetch(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch never faults.
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0)
    };
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above.
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly))
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

#[inline]
fn ptr<N>(l: Link) -> NonNull<N> {
    // SAFETY: object links are non-null.
    unsafe { NonNull::new_unchecked(l as *mut N) }
}

impl<N: Keyed> PtrDict<N> {
    /// A table hashing with `hasher` (tests compare tables seeded alike).
    pub fn with_hasher(hasher: RandomState) -> Self {
        Self { t0: Vec::new(), t1: Vec::new(), rehash_idx: 0, len: 0, hasher, resize: true, _n: PhantomData }
    }

    /// Allow or stop automatic expands and shrinks (`dictTypeResizeAllowed`). Explicit
    /// [`PtrDict::expand`] still applies.
    pub fn set_resize(&mut self, on: bool) {
        self.resize = on;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_rehashing(&self) -> bool {
        !self.t1.is_empty()
    }

    #[cfg(test)]
    pub fn rehash_index(&self) -> usize {
        self.rehash_idx
    }

    /// `dictGetStatsHt` for the main table and, while rehashing, the target table.
    pub fn stats(&self, full: bool) -> Vec<DictStats> {
        let target = chains(&self.t1).sum::<usize>();
        let mut out = vec![DictStats::new(0, self.t0.len(), self.len - target, full.then(|| chains(&self.t0)))];
        if self.is_rehashing() {
            out.push(DictStats::new(1, self.t1.len(), target, full.then(|| chains(&self.t1))));
        }
        out
    }

    /// `dictTryExpand`: make room for `n` elements now (no-op while rehashing, or if fewer than
    /// the elements already there).
    pub fn expand(&mut self, n: usize) {
        if self.is_rehashing() || self.len > n {
            return;
        }
        self.start_resize(n.next_power_of_two().max(MIN_SIZE));
    }

    /// Bytes used by the bucket arrays (Redis' `dictBuckets` × pointer size).
    pub fn table_bytes(&self) -> usize {
        (self.t0.capacity() + self.t1.capacity()) * std::mem::size_of::<Link>()
    }

    /// Buckets of both tables (`dictBuckets`).
    pub fn buckets(&self) -> usize {
        self.t0.len() + self.t1.len()
    }

    #[inline]
    pub fn hash(&self, key: &[u8]) -> u64 {
        self.hasher.hash_one(key)
    }

    /// Prefetch the bucket(s) for hash `h` (first step of a batched lookup).
    #[inline]
    pub fn prefetch_bucket(&self, h: u64) {
        for t in [&self.t0, &self.t1] {
            if !t.is_empty() {
                prefetch(&t[(h as usize) & (t.len() - 1)] as *const Link as *const u8);
            }
        }
    }

    /// Prefetch what the bucket(s) for `h` point at: the object, or the first chain entry.
    #[inline]
    pub fn prefetch_entry(&self, h: u64) {
        for t in [&self.t0, &self.t1] {
            if !t.is_empty() {
                let l = t[(h as usize) & (t.len() - 1)];
                if l != 0 {
                    prefetch((l & !1) as *const u8);
                }
            }
        }
    }

    #[inline]
    fn key_of<'a>(l: Link) -> &'a [u8] {
        // SAFETY: the table only holds live objects.
        unsafe { N::key(ptr(l)) }
    }

    pub fn get(&self, key: &[u8]) -> Option<NonNull<N>> {
        if self.len == 0 {
            return None;
        }
        let h = self.hash(key);
        for t in [&self.t0, &self.t1] {
            if t.is_empty() {
                continue;
            }
            if let Some(l) = walk(t[(h as usize) & (t.len() - 1)]).find(|&l| crate::util::bytes_eq(Self::key_of(l), key)) {
                return Some(ptr(l));
            }
        }
        None
    }

    /// The link holding the object at `l`'s place in its chain: `holder` itself or its chain
    /// entry's object field.
    #[inline]
    fn node_slot(holder: *mut Link) -> *mut Link {
        // SAFETY: `holder` is a live link.
        unsafe {
            if is_chain(*holder) { &raw mut (*chain(*holder)).node } else { holder }
        }
    }

    /// Find the element matching `hit` in table `t`'s bucket for `h`: the link that holds it
    /// (the bucket or the previous chain entry's `next`) and the link before that, if any.
    fn find_in(t: &mut [Link], h: u64, mut hit: impl FnMut(Link) -> bool) -> Option<(*mut Link, *mut Link)> {
        if t.is_empty() {
            return None;
        }
        let mut prev: *mut Link = std::ptr::null_mut();
        let mut holder: *mut Link = &mut t[(h as usize) & (t.len() - 1)];
        // SAFETY: links of the uniquely borrowed table.
        unsafe {
            loop {
                match *holder {
                    0 => return None,
                    l if is_chain(l) => {
                        let c = chain(l);
                        if hit((*c).node) {
                            return Some((holder, prev));
                        }
                        prev = holder;
                        holder = &raw mut (*c).next;
                    }
                    l => return hit(l).then_some((holder, prev)),
                }
            }
        }
    }

    fn find(&mut self, h: u64, mut hit: impl FnMut(Link) -> bool) -> Option<(*mut Link, *mut Link)> {
        Self::find_in(&mut self.t0, h, &mut hit).or_else(|| Self::find_in(&mut self.t1, h, &mut hit))
    }

    /// The link holding `key`'s object (a rehash step first, as Redis' lookups take), so the
    /// caller may change the object or put another in its place.
    pub fn get_slot(&mut self, key: &[u8]) -> Option<&mut Link> {
        if self.len == 0 {
            return None;
        }
        self.rehash_step();
        let h = self.hash(key);
        let (holder, _) = self.find(h, |l| crate::util::bytes_eq(Self::key_of(l), key))?;
        // SAFETY: a link of the table, within the unique borrow of `self`.
        Some(unsafe { &mut *Self::node_slot(holder) })
    }

    /// Point the element at `old` (with this key) at `new` instead (an object moved).
    pub fn repoint(&mut self, key: &[u8], old: NonNull<N>, new: NonNull<N>) {
        let h = self.hash(key);
        let old = old.as_ptr() as Link;
        let (holder, _) = self.find(h, |l| l == old).expect("indexed object");
        // SAFETY: a link of the table.
        unsafe { *Self::node_slot(holder) = new.as_ptr() as Link };
    }

    /// Add an object whose key is known to be absent; returns the link that holds it.
    pub fn insert_new(&mut self, node: NonNull<N>) -> &mut Link {
        self.rehash_step();
        self.expand_if_needed();
        // SAFETY: the caller hands a live object.
        let h = self.hash(unsafe { N::key(node) });
        let t = if self.is_rehashing() { &mut self.t1 } else { &mut self.t0 };
        let idx = (h as usize) & (t.len() - 1);
        let node = node.as_ptr() as Link;
        self.len += 1;
        if t[idx] == 0 {
            t[idx] = node;
            &mut t[idx]
        } else {
            t[idx] = new_chain(node, t[idx]);
            // SAFETY: just allocated.
            unsafe { &mut (*chain(t[idx])).node }
        }
    }

    /// Unlink `key`'s object.
    pub fn remove(&mut self, key: &[u8]) -> Option<NonNull<N>> {
        if self.len == 0 {
            return None;
        }
        self.rehash_step();
        let h = self.hash(key);
        let (holder, prev) = self.find(h, |l| crate::util::bytes_eq(Self::key_of(l), key))?;
        Some(self.unlink(holder, prev))
    }

    /// Unlink the object at `p` (with this key): a TTL index dropping a key.
    pub fn remove_ptr(&mut self, key: &[u8], p: NonNull<N>) {
        self.rehash_step();
        let h = self.hash(key);
        let p = p.as_ptr() as Link;
        let (holder, prev) = self.find(h, |l| l == p).expect("indexed object");
        self.unlink(holder, prev);
    }

    fn unlink(&mut self, holder: *mut Link, prev: *mut Link) -> NonNull<N> {
        // SAFETY: `holder` and `prev` come from `find` on this table, unchanged since.
        let node = unsafe {
            let l = *holder;
            if is_chain(l) {
                let c = chain(l);
                *holder = (*c).next;
                let node = (*c).node;
                free_chain(l);
                node
            } else {
                *holder = 0;
                // The chain entry before it is now the last: its object goes in its link.
                if !prev.is_null() {
                    let pl = *prev;
                    *prev = (*chain(pl)).node;
                    free_chain(pl);
                }
                l
            }
        };
        self.len -= 1;
        self.shrink_if_needed();
        ptr(node)
    }

    /// Forget every element (the objects are the owner's to free).
    pub fn clear(&mut self) {
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut l = std::mem::take(b);
                while is_chain(l) {
                    // SAFETY: a live chain entry, freed once.
                    let next = unsafe { (*chain(l)).next };
                    unsafe { free_chain(l) };
                    l = next;
                }
            }
        }
        self.t0 = Vec::new();
        self.t1 = Vec::new();
        self.rehash_idx = 0;
        self.len = 0;
    }

    /// Every object, table by table, bucket by bucket.
    pub fn iter(&self) -> impl Iterator<Item = NonNull<N>> + '_ {
        self.t0.iter().chain(self.t1.iter()).flat_map(|&b| walk(b)).map(ptr)
    }

    /// Visit every element's link (the object may be changed or put elsewhere, not removed).
    pub fn for_each_slot(&mut self, mut f: impl FnMut(&mut Link)) {
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut holder: *mut Link = b;
                // SAFETY: links of the uniquely borrowed table; `f` only replaces objects.
                unsafe {
                    while *holder != 0 {
                        let l = *holder;
                        f(&mut *Self::node_slot(holder));
                        if !is_chain(l) {
                            break;
                        }
                        holder = &raw mut (*chain(l)).next;
                    }
                }
            }
        }
    }

    /// One SCAN step: calls `f` for every element of the bucket(s) at `cursor` and returns the
    /// next cursor (0 when the iteration is complete).
    pub fn scan(&self, cursor: u64, mut f: impl FnMut(NonNull<N>)) -> u64 {
        if self.len == 0 {
            return 0;
        }
        let mut v = cursor;
        let mut emit = |t: &Vec<Link>, idx: u64| walk(t[idx as usize]).for_each(|l| f(ptr(l)));
        if !self.is_rehashing() {
            let m0 = (self.t0.len() - 1) as u64;
            emit(&self.t0, v & m0);
            v |= !m0;
            v = v.reverse_bits().wrapping_add(1).reverse_bits();
        } else {
            let (small, large) =
                if self.t0.len() <= self.t1.len() { (&self.t0, &self.t1) } else { (&self.t1, &self.t0) };
            let m0 = (small.len() - 1) as u64;
            let m1 = (large.len() - 1) as u64;
            emit(small, v & m0);
            loop {
                emit(large, v & m1);
                v |= !m1;
                v = v.reverse_bits().wrapping_add(1).reverse_bits();
                if v & (m0 ^ m1) == 0 {
                    break;
                }
            }
        }
        v
    }

    /// A random element (bucket-uniform, like `dictGetRandomKey`).
    pub fn random(&self, rng: &mut Rng) -> Option<NonNull<N>> {
        if self.len == 0 {
            return None;
        }
        let (n0, n1) = (self.t0.len(), self.t1.len());
        let head = loop {
            let i = if self.is_rehashing() {
                // Buckets of t0 below the rehash index are empty.
                self.rehash_idx + rng.below(n0 + n1 - self.rehash_idx)
            } else {
                rng.below(n0)
            };
            let b = if i < n0 { self.t0[i] } else { self.t1[i - n0] };
            if b != 0 {
                break b;
            }
        };
        let len = walk(head).count();
        walk(head).nth(rng.below(len)).map(ptr)
    }

    /// Up to `count` elements from consecutive buckets starting at a random one
    /// (`dictGetSomeKeys`): cheap sampling for eviction and active expiry.
    pub fn sample(&self, rng: &mut Rng, count: usize, mut f: impl FnMut(NonNull<N>)) {
        if self.len == 0 || count == 0 {
            return;
        }
        let count = count.min(self.len);
        let tables: Vec<&Vec<Link>> = [&self.t0, &self.t1].into_iter().filter(|t| !t.is_empty()).collect();
        let maxsize = tables.iter().map(|t| t.len()).max().unwrap();
        let mut i = rng.below(maxsize);
        let mut got = 0;
        let mut empty = 0;
        let mut steps = count * 10;
        while got < count && steps > 0 {
            steps -= 1;
            for (ti, t) in tables.iter().enumerate() {
                // Skip already-rehashed buckets of t0.
                if ti == 0 && self.is_rehashing() && i < self.rehash_idx {
                    continue;
                }
                if i >= t.len() {
                    continue;
                }
                let b = t[i];
                if b == 0 {
                    empty += 1;
                    if empty >= 5 && empty > count {
                        i = rng.below(maxsize);
                        empty = 0;
                    }
                }
                for l in walk(b) {
                    f(ptr(l));
                    got += 1;
                    if got >= count {
                        return;
                    }
                }
            }
            i = (i + 1) & (maxsize - 1);
        }
    }

    /// Move up to `n` buckets from the old table. Returns true while more rehashing is pending.
    pub fn rehash(&mut self, n: usize) -> bool {
        if !self.is_rehashing() {
            // Idle tables get resized from the cron, like Redis' tryResizeHashTables.
            self.shrink_if_needed();
            if !self.is_rehashing() {
                return false;
            }
        }
        let mut empty_visits = n * EMPTY_VISITS;
        let mut n = n;
        while n > 0 && self.rehash_idx < self.t0.len() {
            n -= 1;
            while self.t0[self.rehash_idx] == 0 {
                self.rehash_idx += 1;
                empty_visits -= 1;
                if self.rehash_idx >= self.t0.len() {
                    break;
                }
                if empty_visits == 0 {
                    return true;
                }
            }
            if self.rehash_idx >= self.t0.len() {
                break;
            }
            let mask = self.t1.len() - 1;
            let mut l = std::mem::take(&mut self.t0[self.rehash_idx]);
            // Each object goes to the head of its new bucket, in chain order (as dict does);
            // chain entries are reused where one is needed.
            while l != 0 {
                let (node, entry, next) = if is_chain(l) {
                    // SAFETY: a live chain entry, moved once.
                    let c = unsafe { &*chain(l) };
                    (c.node, l, c.next)
                } else {
                    (l, 0, 0)
                };
                let idx = (self.hash(Self::key_of(node)) as usize) & mask;
                let b = &mut self.t1[idx];
                if *b == 0 {
                    if entry != 0 {
                        // SAFETY: the entry was unlinked above.
                        unsafe { free_chain(entry) };
                    }
                    *b = node;
                } else if entry != 0 {
                    // SAFETY: as above, now relinked.
                    unsafe { (*chain(entry)).next = *b };
                    *b = entry;
                } else {
                    *b = new_chain(node, *b);
                }
                l = next;
            }
            self.rehash_idx += 1;
        }
        if self.rehash_idx >= self.t0.len() {
            self.t0 = std::mem::take(&mut self.t1);
            self.rehash_idx = 0;
            // Elements removed during the rehash may call for a further shrink.
            self.shrink_if_needed();
            return self.is_rehashing();
        }
        true
    }

    #[inline]
    fn rehash_step(&mut self) {
        if self.is_rehashing() {
            self.rehash(1);
        }
    }

    fn start_resize(&mut self, size: usize) {
        if self.t0.is_empty() {
            self.t0 = vec![0; size];
            return;
        }
        if self.is_rehashing() || size == self.t0.len() {
            return;
        }
        self.t1 = vec![0; size];
        self.rehash_idx = 0;
    }

    fn expand_if_needed(&mut self) {
        if self.is_rehashing() {
            return;
        }
        if self.t0.is_empty() {
            self.t0 = vec![0; MIN_SIZE];
        } else if self.resize && self.len >= self.t0.len() {
            self.start_resize((self.len + 1).next_power_of_two());
        }
    }

    fn shrink_if_needed(&mut self) {
        if self.is_rehashing() || self.t0.len() <= MIN_SIZE || !self.resize {
            return;
        }
        if self.len == 0 {
            self.t0 = Vec::new();
            return;
        }
        // Shrink when less than 1/8 full (Redis 7.4+).
        if self.len * 8 <= self.t0.len() {
            self.start_resize(self.len.next_power_of_two().max(MIN_SIZE));
        }
    }
}

impl<N: Keyed> Drop for PtrDict<N> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Each bucket's chain length.
fn chains(t: &[Link]) -> impl Iterator<Item = usize> + '_ {
    t.iter().map(|&b| walk(b).count())
}
