//! [`Str`]: a byte string in 24 bytes, holding up to 23 bytes inline (most keys, members, fields
//! and small values need no allocation of their own) and longer ones in a heap buffer.
//!
//! Layout: bytes 0..23 hold the data inline, or a heap buffer's pointer (0..8), length (8..16)
//! and capacity (16..23); byte 23 is the inline length (0..=23) or [`Tag::Heap`]. That last
//! byte's unused values are a niche: an enum holding a `Str` among boxed variants (`Value`) is 24
//! bytes, not 32.

use std::alloc::{Layout, alloc, dealloc, realloc};

const INLINE: usize = 23;
/// Capacities fit the 7 bytes they are stored in.
const MAX_CAP: usize = (1 << 56) - 1;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
enum Tag {
    L0 = 0,
    L1 = 1,
    L2 = 2,
    L3 = 3,
    L4 = 4,
    L5 = 5,
    L6 = 6,
    L7 = 7,
    L8 = 8,
    L9 = 9,
    L10 = 10,
    L11 = 11,
    L12 = 12,
    L13 = 13,
    L14 = 14,
    L15 = 15,
    L16 = 16,
    L17 = 17,
    L18 = 18,
    L19 = 19,
    L20 = 20,
    L21 = 21,
    L22 = 22,
    L23 = 23,
    Heap = 255,
}

impl Tag {
    #[inline]
    fn inline(len: usize) -> Tag {
        debug_assert!(len <= INLINE);
        // SAFETY: 0..=23 are variants.
        unsafe { std::mem::transmute::<u8, Tag>(len as u8) }
    }
}

#[repr(C)]
pub struct Str {
    bytes: [u8; INLINE],
    tag: Tag,
}

// SAFETY: a `Str` owns its buffer like a `Vec<u8>`.
unsafe impl Send for Str {}
// SAFETY: as above; shared access is read-only.
unsafe impl Sync for Str {}

fn layout(cap: usize) -> Layout {
    assert!(cap <= MAX_CAP, "Str capacity overflow");
    Layout::array::<u8>(cap).expect("Str capacity overflow")
}

impl Str {
    #[inline]
    pub const fn new() -> Self {
        Str { bytes: [0; INLINE], tag: Tag::L0 }
    }

    /// Whether the bytes are in a heap buffer.
    #[inline]
    pub fn spilled(&self) -> bool {
        self.tag == Tag::Heap
    }

    // ---- heap fields, stored as bytes (the pointer keeps its provenance: written and read as a
    // pointer) ----

    #[inline]
    fn heap_ptr(&self) -> *mut u8 {
        // SAFETY: bytes 0..8 hold a pointer written by `set_heap`.
        unsafe { std::ptr::read_unaligned(self.bytes.as_ptr() as *const *mut u8) }
    }

    #[inline]
    fn heap_len(&self) -> usize {
        usize::from_le_bytes(self.bytes[8..16].try_into().unwrap())
    }

    #[inline]
    fn heap_cap(&self) -> usize {
        let mut b = [0u8; 8];
        b[..7].copy_from_slice(&self.bytes[16..23]);
        usize::from_le_bytes(b)
    }

    #[inline]
    fn set_heap_len(&mut self, len: usize) {
        self.bytes[8..16].copy_from_slice(&len.to_le_bytes());
    }

    fn set_heap(&mut self, ptr: *mut u8, len: usize, cap: usize) {
        // SAFETY: bytes 0..8 take the pointer.
        unsafe { std::ptr::write_unaligned(self.bytes.as_mut_ptr() as *mut *mut u8, ptr) };
        self.set_heap_len(len);
        self.bytes[16..23].copy_from_slice(&cap.to_le_bytes()[..7]);
        self.tag = Tag::Heap;
    }

    #[inline]
    pub fn len(&self) -> usize {
        if self.spilled() { self.heap_len() } else { self.tag as usize }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        if self.spilled() { self.heap_cap() } else { INLINE }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        if self.spilled() {
            // SAFETY: the heap buffer's first `len` bytes are initialised.
            unsafe { std::slice::from_raw_parts(self.heap_ptr(), self.heap_len()) }
        } else {
            &self.bytes[..self.tag as usize]
        }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.spilled() {
            // SAFETY: as in `as_slice`, through a unique borrow.
            unsafe { std::slice::from_raw_parts_mut(self.heap_ptr(), self.heap_len()) }
        } else {
            let n = self.tag as usize;
            &mut self.bytes[..n]
        }
    }

    pub fn from_slice(s: &[u8]) -> Self {
        let mut out = Self::new();
        if s.len() <= INLINE {
            out.bytes[..s.len()].copy_from_slice(s);
            out.tag = Tag::inline(s.len());
        } else {
            out.spill_to(s.len(), s);
        }
        out
    }

    /// Take a vector's bytes: inline if they fit, else its own buffer (no copy).
    pub fn from_vec(v: Vec<u8>) -> Self {
        if v.len() <= INLINE || v.capacity() > MAX_CAP {
            return Str::from_slice(&v);
        }
        let mut v = std::mem::ManuallyDrop::new(v);
        let (len, cap) = (v.len(), v.capacity());
        let mut out = Str::new();
        // A non-empty Vec<u8>'s buffer was allocated with `Layout::array::<u8>(cap)`, as `free`
        // releases it; the Vec is forgotten.
        out.set_heap(v.as_mut_ptr(), len, cap);
        out
    }

    /// `n` copies of `b`.
    pub fn from_elem(b: u8, n: usize) -> Self {
        let mut s = Str::new();
        s.resize(n, b);
        s
    }

    pub fn with_capacity(cap: usize) -> Self {
        let mut s = Self::new();
        s.reserve(cap);
        s
    }

    /// Become a heap string of capacity `cap` holding `data` (from wherever it is now).
    fn spill_to(&mut self, cap: usize, data: &[u8]) {
        debug_assert!(cap >= data.len() && cap > 0);
        // SAFETY: `cap` is non-zero; `data` is copied before the old buffer is freed.
        unsafe {
            let ptr = alloc(layout(cap));
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout(cap));
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            self.free();
            self.set_heap(ptr, data.len(), cap);
        }
    }

    /// Release a heap buffer (the string is then empty and inline).
    fn free(&mut self) {
        if self.spilled() {
            // SAFETY: spilled: the buffer was allocated with this capacity.
            unsafe { dealloc(self.heap_ptr(), layout(self.heap_cap())) };
            self.tag = Tag::L0;
        }
    }

    /// Room for `add` more bytes, growing at least twofold.
    pub fn reserve(&mut self, add: usize) {
        let len = self.len();
        let need = len.checked_add(add).expect("Str capacity overflow");
        if need <= self.capacity() {
            return;
        }
        let cap = need.max(self.capacity() * 2);
        self.grow_to(cap);
    }

    /// Room for exactly `add` more bytes.
    pub fn reserve_exact(&mut self, add: usize) {
        let need = self.len().checked_add(add).expect("Str capacity overflow");
        if need > self.capacity() {
            self.grow_to(need);
        }
    }

    fn grow_to(&mut self, cap: usize) {
        if self.spilled() {
            let (old, len) = (self.heap_cap(), self.heap_len());
            // SAFETY: the buffer was allocated with `old`; `cap` > `old` > 0.
            unsafe {
                let ptr = realloc(self.heap_ptr(), layout(old), cap);
                if ptr.is_null() {
                    std::alloc::handle_alloc_error(layout(cap));
                }
                self.set_heap(ptr, len, cap);
            }
        } else {
            let mut tmp = [0u8; INLINE];
            let n = self.tag as usize;
            tmp[..n].copy_from_slice(&self.bytes[..n]);
            self.spill_to(cap, &tmp[..n]);
        }
    }

    /// Set the length.
    ///
    /// # Safety
    /// `len` must not exceed the capacity, and bytes `..len` must be initialised.
    unsafe fn set_len(&mut self, len: usize) {
        if self.spilled() {
            self.set_heap_len(len);
        } else {
            self.tag = Tag::inline(len);
        }
    }

    fn data_ptr(&mut self) -> *mut u8 {
        if self.spilled() { self.heap_ptr() } else { self.bytes.as_mut_ptr() }
    }

    pub fn push(&mut self, b: u8) {
        self.extend_from_slice(&[b]);
    }

    pub fn extend_from_slice(&mut self, s: &[u8]) {
        self.reserve(s.len());
        let len = self.len();
        // SAFETY: reserved room for `s` after `len`.
        unsafe {
            std::ptr::copy_nonoverlapping(s.as_ptr(), self.data_ptr().add(len), s.len());
            self.set_len(len + s.len());
        }
    }

    pub fn insert_from_slice(&mut self, at: usize, s: &[u8]) {
        let len = self.len();
        assert!(at <= len, "insert position out of bounds");
        self.reserve(s.len());
        // SAFETY: reserved; the tail moves within the buffer, then `s` fills the gap.
        unsafe {
            let p = self.data_ptr();
            std::ptr::copy(p.add(at), p.add(at + s.len()), len - at);
            std::ptr::copy_nonoverlapping(s.as_ptr(), p.add(at), s.len());
            self.set_len(len + s.len());
        }
    }

    pub fn truncate(&mut self, len: usize) {
        if len < self.len() {
            // SAFETY: shrinking keeps initialised bytes.
            unsafe { self.set_len(len) };
        }
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    pub fn resize(&mut self, len: usize, v: u8) {
        let cur = self.len();
        if len <= cur {
            self.truncate(len);
            return;
        }
        self.reserve(len - cur);
        // SAFETY: reserved; the new bytes are written before the length covers them.
        unsafe {
            std::ptr::write_bytes(self.data_ptr().add(cur), v, len - cur);
            self.set_len(len);
        }
    }

    /// Back inline when it fits, else down to an exact heap buffer.
    pub fn shrink_to_fit(&mut self) {
        if !self.spilled() {
            return;
        }
        let len = self.heap_len();
        if len <= INLINE {
            let mut tmp = [0u8; INLINE];
            tmp[..len].copy_from_slice(self.as_slice());
            self.free();
            self.bytes[..len].copy_from_slice(&tmp[..len]);
            self.tag = Tag::inline(len);
        } else if len < self.heap_cap() {
            let old = self.heap_cap();
            // SAFETY: as in `grow_to`, to a smaller non-zero size.
            unsafe {
                let ptr = realloc(self.heap_ptr(), layout(old), len);
                if ptr.is_null() {
                    std::alloc::handle_alloc_error(layout(len));
                }
                self.set_heap(ptr, len, len);
            }
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    /// Remove and return the byte at `i`.
    pub fn remove(&mut self, i: usize) -> u8 {
        let len = self.len();
        assert!(i < len, "remove index out of bounds");
        let s = self.as_mut_slice();
        let b = s[i];
        s.copy_within(i + 1.., i);
        // SAFETY: shrinking by one.
        unsafe { self.set_len(len - 1) };
        b
    }

    pub fn pop(&mut self) -> Option<u8> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let b = self.as_slice()[len - 1];
        // SAFETY: shrinking by one.
        unsafe { self.set_len(len - 1) };
        Some(b)
    }

    /// Remove `range`, returning nothing (unlike `Vec::drain`).
    pub fn drain(&mut self, range: impl std::ops::RangeBounds<usize>) {
        use std::ops::Bound;
        let len = self.len();
        let start = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => len,
        };
        assert!(start <= end && end <= len, "drain range out of bounds");
        self.as_mut_slice().copy_within(end.., start);
        // SAFETY: shrinking.
        unsafe { self.set_len(len - (end - start)) };
    }
}

impl Drop for Str {
    fn drop(&mut self) {
        self.free();
    }
}

impl Clone for Str {
    fn clone(&self) -> Self {
        Str::from_slice(self.as_slice())
    }
}

impl Default for Str {
    fn default() -> Self {
        Str::new()
    }
}

impl std::ops::Deref for Str {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for Str {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl AsRef<[u8]> for Str {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsMut<[u8]> for Str {
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl std::borrow::Borrow<[u8]> for Str {
    #[inline]
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::borrow::BorrowMut<[u8]> for Str {
    fn borrow_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl std::hash::Hash for Str {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state)
    }
}

impl PartialEq for Str {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Str {}

impl PartialOrd for Str {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Str {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_slice().cmp(other.as_slice())
    }
}

impl std::fmt::Debug for Str {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl From<&[u8]> for Str {
    fn from(s: &[u8]) -> Self {
        Str::from_slice(s)
    }
}

impl<const N: usize> From<[u8; N]> for Str {
    fn from(s: [u8; N]) -> Self {
        Str::from_slice(&s)
    }
}

impl From<Vec<u8>> for Str {
    fn from(v: Vec<u8>) -> Self {
        Str::from_slice(&v)
    }
}

impl From<&Vec<u8>> for Str {
    fn from(v: &Vec<u8>) -> Self {
        Str::from_slice(v)
    }
}

impl FromIterator<u8> for Str {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        let mut s = Str::new();
        s.extend(iter);
        s
    }
}

impl<'a> FromIterator<&'a u8> for Str {
    fn from_iter<I: IntoIterator<Item = &'a u8>>(iter: I) -> Self {
        iter.into_iter().copied().collect()
    }
}

impl Extend<u8> for Str {
    fn extend<I: IntoIterator<Item = u8>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        self.reserve(iter.size_hint().0);
        for b in iter {
            self.push(b);
        }
    }
}

impl<'a> Extend<&'a u8> for Str {
    fn extend<I: IntoIterator<Item = &'a u8>>(&mut self, iter: I) {
        self.extend(iter.into_iter().copied())
    }
}

impl std::io::Write for Str {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl IntoIterator for Str {
    type Item = u8;
    type IntoIter = std::vec::IntoIter<u8>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl<'a> IntoIterator for &'a Str {
    type Item = &'a u8;
    type IntoIter = std::slice::Iter<'a, u8>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_24_bytes_with_a_niche() {
        assert_eq!(std::mem::size_of::<Str>(), 24);
        assert_eq!(std::mem::size_of::<Option<Str>>(), 24);
        assert_eq!(std::mem::size_of::<crate::value::Value>(), 24);
    }

    #[test]
    fn matches_a_vec() {
        let mut x = 0x2545F4914F6CDD1Du64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for _ in 0..200 {
            let mut s = Str::new();
            let mut v: Vec<u8> = Vec::new();
            for _ in 0..200 {
                match next(11) {
                    0 => {
                        let b = next(256) as u8;
                        s.push(b);
                        v.push(b);
                    }
                    1 => {
                        let n = next(40) as usize;
                        let d: Vec<u8> = (0..n).map(|i| i as u8).collect();
                        s.extend_from_slice(&d);
                        v.extend_from_slice(&d);
                    }
                    2 => {
                        let n = next(v.len() as u64 + 1) as usize;
                        s.truncate(n);
                        v.truncate(n);
                    }
                    3 => {
                        let at = next(v.len() as u64 + 1) as usize;
                        let d = [7u8; 5];
                        s.insert_from_slice(at, &d);
                        v.splice(at..at, d);
                    }
                    4 => {
                        let n = next(60) as usize;
                        s.resize(n, 9);
                        v.resize(n, 9);
                    }
                    5 => s.shrink_to_fit(),
                    6 => {
                        s = s.clone();
                    }
                    7 if !v.is_empty() => {
                        let i = next(v.len() as u64) as usize;
                        assert_eq!(s.remove(i), v.remove(i));
                    }
                    8 => {
                        let a = next(v.len() as u64 + 1) as usize;
                        let b = a + next((v.len() - a) as u64 + 1) as usize;
                        s.drain(a..b);
                        v.drain(a..b);
                    }
                    9 => {
                        s = Str::from_vec(v.clone());
                    }
                    _ => {
                        s = Str::from_slice(&v);
                    }
                }
                assert_eq!(s.as_slice(), &v[..]);
                assert_eq!(s.len(), v.len());
                assert!(s.capacity() >= s.len());
                assert_eq!(s.spilled(), s.capacity() > INLINE);
            }
        }
    }

    #[test]
    fn hashes_like_a_slice() {
        use std::hash::{BuildHasher, BuildHasherDefault};
        let h = BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default();
        for n in [0, 5, 23, 24, 100] {
            let v: Vec<u8> = (0..n).map(|i| i as u8).collect();
            assert_eq!(h.hash_one(Str::from_slice(&v)), h.hash_one(&v[..]));
        }
    }
}
