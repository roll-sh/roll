//! Byte strings to byte strings (a hash's fields once it is a hashtable): Redis' dict, each pair
//! one allocation (the two lengths, the field, the value) in a [`PtrDict`], where boxing two
//! [`crate::value::Str`]s and a chain link took 56 bytes before the bytes themselves. It behaves
//! exactly like the `Dict<Str, Str>` it replaces: order, SCAN, random picks, rehash steps.

use crate::dict::DictStats;
use crate::ptrdict::{Keyed, PtrDict};
use crate::util::Rng;
use std::alloc::{Layout, alloc, dealloc};
use std::ptr::NonNull;

/// A pair's header; the field's bytes and then the value's follow it.
#[repr(C)]
pub struct Pair {
    flen: u32,
    vlen: u32,
}

const HEAD: usize = std::mem::size_of::<Pair>();

#[inline]
fn layout(flen: usize, vlen: usize) -> Layout {
    Layout::from_size_align(HEAD + flen + vlen, 8).expect("pair layout")
}

fn pair_new(f: &[u8], v: &[u8]) -> NonNull<Pair> {
    let l = layout(f.len(), v.len());
    // SAFETY: the layout is non-zero sized; header and bytes are written before use.
    unsafe {
        let base = alloc(l);
        let Some(p) = NonNull::new(base as *mut Pair) else { std::alloc::handle_alloc_error(l) };
        p.as_ptr().write(Pair { flen: u32::try_from(f.len()).expect("field length"), vlen: u32::try_from(v.len()).expect("value length") });
        std::ptr::copy_nonoverlapping(f.as_ptr(), base.add(HEAD), f.len());
        std::ptr::copy_nonoverlapping(v.as_ptr(), base.add(HEAD + f.len()), v.len());
        p
    }
}

/// # Safety
/// `p` is a live pair no table points at any more.
unsafe fn pair_free(p: NonNull<Pair>) {
    // SAFETY: a live pair, allocated with this layout.
    unsafe {
        let h = p.as_ref();
        dealloc(p.as_ptr() as *mut u8, layout(h.flen as usize, h.vlen as usize));
    }
}

#[inline]
fn field<'a>(p: NonNull<Pair>) -> &'a [u8] {
    // SAFETY: a live pair's field bytes follow its header.
    unsafe { std::slice::from_raw_parts((p.as_ptr() as *const u8).add(HEAD), p.as_ref().flen as usize) }
}

#[inline]
fn value<'a>(p: NonNull<Pair>) -> &'a [u8] {
    // SAFETY: a live pair's value bytes follow its field.
    unsafe {
        let h = p.as_ref();
        std::slice::from_raw_parts((p.as_ptr() as *const u8).add(HEAD + h.flen as usize), h.vlen as usize)
    }
}

// SAFETY: a pair's field is fixed for its life.
unsafe impl Keyed for Pair {
    #[inline]
    unsafe fn key<'a>(p: NonNull<Self>) -> &'a [u8] {
        field(p)
    }
}

#[derive(Default)]
pub struct PairMap {
    d: PtrDict<Pair>,
}

impl PairMap {
    /// Sized for `n` pairs (`dictCreate` + `dictExpand`).
    pub fn with_capacity(n: usize) -> Self {
        let mut d = PtrDict::default();
        if n > 0 {
            d.expand(n);
        }
        Self { d }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.d.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.d.is_empty()
    }

    #[inline]
    pub fn get(&self, f: &[u8]) -> Option<&[u8]> {
        self.d.get(f).map(value)
    }

    #[inline]
    pub fn contains_key(&self, f: &[u8]) -> bool {
        self.d.get(f).is_some()
    }

    /// The value's length, looked up as a write does (a rehash step).
    pub fn value_len_mut(&mut self, f: &[u8]) -> Option<usize> {
        self.d.get_slot(f).map(|slot| value(ptr(*slot)).len())
    }

    /// Replace `f`'s value (a rehash step); false if there is no such field.
    pub fn set_value(&mut self, f: &[u8], v: &[u8]) -> bool {
        let Some(slot) = self.d.get_slot(f) else { return false };
        let old = ptr(*slot);
        if value(old).len() == v.len() {
            // SAFETY: the pair has `v.len()` value bytes after its field (written through the
            // allocation's own pointer).
            unsafe {
                let at = (old.as_ptr() as *mut u8).add(HEAD + field(old).len());
                std::ptr::copy_nonoverlapping(v.as_ptr(), at, v.len());
            }
        } else {
            *slot = pair_new(field(old), v).as_ptr() as usize;
            // SAFETY: the table points at the new pair now.
            unsafe { pair_free(old) };
        }
        true
    }

    /// Add a field known to be absent.
    pub fn insert_new(&mut self, f: &[u8], v: &[u8]) {
        self.d.insert_new(pair_new(f, v));
    }

    /// Remove `f`; its field and value lengths.
    pub fn remove(&mut self, f: &[u8]) -> Option<(usize, usize)> {
        let p = self.d.remove(f)?;
        let lens = (field(p).len(), value(p).len());
        // SAFETY: unlinked.
        unsafe { pair_free(p) };
        Some(lens)
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter { it: Box::new(self.d.iter()), left: self.d.len() }
    }

    pub fn random(&self, rng: &mut Rng) -> Option<(&[u8], &[u8])> {
        self.d.random(rng).map(|p| (field(p), value(p)))
    }

    pub fn scan(&self, cursor: u64, mut f: impl FnMut(&[u8], &[u8])) -> u64 {
        self.d.scan(cursor, |p| f(field(p), value(p)))
    }

    pub fn stats(&self, full: bool) -> Vec<DictStats> {
        self.d.stats(full)
    }

    pub fn table_bytes(&self) -> usize {
        self.d.table_bytes()
    }

    pub fn buckets(&self) -> usize {
        self.d.buckets()
    }
}

#[inline]
fn ptr(l: usize) -> NonNull<Pair> {
    // SAFETY: table slots hold live pair pointers.
    unsafe { NonNull::new_unchecked(l as *mut Pair) }
}

impl Drop for PairMap {
    fn drop(&mut self) {
        for p in self.d.iter() {
            // SAFETY: each pair once; the table is cleared next.
            unsafe { pair_free(p) };
        }
        self.d.clear();
    }
}

/// A copy as `Dict`'s: the pairs inserted in iteration order into a table sized for them.
impl Clone for PairMap {
    fn clone(&self) -> Self {
        let mut m = PairMap::with_capacity(self.len());
        for (f, v) in self.iter() {
            m.insert_new(f, v);
        }
        m
    }
}

impl std::fmt::Debug for PairMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The pairs, in table order.
pub struct Iter<'a> {
    it: Box<dyn Iterator<Item = NonNull<Pair>> + 'a>,
    left: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let p = self.it.next()?;
        self.left -= 1;
        Some((field(p), value(p)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dict;
    use crate::value::Str;

    /// Same contents, order, SCAN and table shape as the `Dict<Str, Str>` it replaces.
    #[test]
    fn matches_dict() {
        let mut m = PairMap::default();
        let mut d: Dict<Str, Str> = Dict::new();
        let mut r = Rng::seeded(3);
        for step in 0..20_000u64 {
            let f = format!("field:{}", r.next_u64() % 2000).into_bytes();
            let v = vec![b'v'; (r.next_u64() % 40) as usize];
            match r.next_u64() % 4 {
                0 | 1 => {
                    if d.contains_key(&f) {
                        assert!(m.set_value(&f, &v));
                        *d.get_mut(&f).unwrap() = Str::from_slice(&v);
                    } else {
                        m.insert_new(&f, &v);
                        d.insert_new(Str::from_slice(&f), Str::from_slice(&v));
                    }
                }
                2 => assert_eq!(m.remove(&f), d.remove_entry(&f).map(|(k, v)| (k.len(), v.len()))),
                _ => assert_eq!(m.get(&f), d.get(&f).map(|v| v.as_slice())),
            }
            if step % 2000 == 0 {
                let mut a: Vec<_> = m.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect();
                let mut b: Vec<_> = d.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect();
                a.sort();
                b.sort();
                assert_eq!(a, b);
                assert_eq!((m.len(), m.buckets(), m.table_bytes()), (d.len(), d.buckets(), d.table_bytes()));
                assert_eq!(m.iter().size_hint().0, m.len());
            }
        }
        let c = m.clone();
        assert_eq!(c.len(), m.len());
        assert!(c.iter().all(|(f, v)| m.get(f) == Some(v)));
    }
}
