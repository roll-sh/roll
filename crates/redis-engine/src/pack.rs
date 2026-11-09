//! `Pack`: a compact sequence of byte strings in one allocation, the equivalent of Redis'
//! listpack. Small lists, hashes, sets and sorted sets live in a `Pack` (about 1–2 bytes of
//! overhead per element instead of a pointer-heavy node) and convert to a real structure when
//! they outgrow the `*-max-listpack-*` limits. Operations are O(n), which is fast for n ≤ ~128.
//!
//! Entry encoding: LEB128 length, then the bytes.

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Pack {
    buf: Vec<u8>,
    len: u32,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter().map(String::from_utf8_lossy)).finish()
    }
}

#[inline]
fn varint_len(mut n: usize) -> usize {
    let mut l = 1;
    while n >= 0x80 {
        n >>= 7;
        l += 1;
    }
    l
}

#[inline]
fn write_varint(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

#[inline]
fn read_varint(buf: &[u8], pos: usize) -> (usize, usize) {
    let mut n = 0usize;
    let mut shift = 0;
    let mut p = pos;
    loop {
        let b = buf[p];
        n |= ((b & 0x7f) as usize) << shift;
        p += 1;
        if b < 0x80 {
            return (n, p);
        }
        shift += 7;
    }
}

fn encode(v: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(v.len() + varint_len(v.len()));
    write_varint(&mut e, v.len());
    e.extend_from_slice(v);
    e
}

impl Pack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_items<I: IntoIterator<Item = T>, T: AsRef<[u8]>>(items: I) -> Self {
        let mut p = Self::new();
        for i in items {
            p.push_back(i.as_ref());
        }
        p
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes of the encoded contents.
    pub fn bytes(&self) -> usize {
        self.buf.len()
    }

    /// Heap bytes held (for memory accounting).
    pub fn mem(&self) -> usize {
        self.buf.capacity()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.len = 0;
    }

    pub fn shrink_to_fit(&mut self) {
        self.buf.shrink_to_fit();
    }

    pub fn iter(&self) -> PackIter<'_> {
        PackIter { buf: &self.buf, pos: 0, left: self.len as usize }
    }

    /// Iterate `(offset, value)` so callers can edit at a position found by scanning.
    pub fn iter_offsets(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let mut pos = 0;
        let buf = &self.buf;
        std::iter::from_fn(move || {
            if pos >= buf.len() {
                return None;
            }
            let at = pos;
            let (n, p) = read_varint(buf, pos);
            pos = p + n;
            Some((at, &buf[p..p + n]))
        })
    }

    /// Byte offset of element `i` (or the end when `i == len`).
    fn offset(&self, i: usize) -> usize {
        let mut pos = 0;
        for _ in 0..i {
            let (n, p) = read_varint(&self.buf, pos);
            pos = p + n;
        }
        pos
    }

    /// The element at byte offset `at`, and the offset after it.
    fn at(&self, at: usize) -> (&[u8], usize) {
        let (n, p) = read_varint(&self.buf, at);
        (&self.buf[p..p + n], p + n)
    }

    pub fn get(&self, i: usize) -> Option<&[u8]> {
        if i >= self.len() {
            return None;
        }
        Some(self.at(self.offset(i)).0)
    }

    pub fn first(&self) -> Option<&[u8]> {
        self.get(0)
    }

    pub fn last(&self) -> Option<&[u8]> {
        if self.is_empty() { None } else { self.get(self.len() - 1) }
    }

    pub fn push_back(&mut self, v: &[u8]) {
        write_varint(&mut self.buf, v.len());
        self.buf.extend_from_slice(v);
        self.len += 1;
    }

    pub fn push_front(&mut self, v: &[u8]) {
        self.insert(0, v);
    }

    /// Insert before element `i` (`i == len` appends).
    pub fn insert(&mut self, i: usize, v: &[u8]) {
        let at = self.offset(i.min(self.len()));
        let e = encode(v);
        self.buf.splice(at..at, e);
        self.len += 1;
    }

    pub fn remove(&mut self, i: usize) -> Option<Vec<u8>> {
        if i >= self.len() {
            return None;
        }
        let at = self.offset(i);
        let (v, end) = self.at(at);
        let v = v.to_vec();
        self.buf.drain(at..end);
        self.len -= 1;
        Some(v)
    }

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        self.remove(0)
    }

    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        if self.is_empty() { None } else { self.remove(self.len() - 1) }
    }

    /// Replace element `i`.
    pub fn set(&mut self, i: usize, v: &[u8]) -> bool {
        if i >= self.len() {
            return false;
        }
        let at = self.offset(i);
        let (_, end) = self.at(at);
        self.buf.splice(at..end, encode(v));
        true
    }

    /// Remove `count` elements starting at `i`.
    pub fn remove_range(&mut self, i: usize, count: usize) {
        if i >= self.len() || count == 0 {
            return;
        }
        let count = count.min(self.len() - i);
        let start = self.offset(i);
        let mut end = start;
        for _ in 0..count {
            end = self.at(end).1;
        }
        self.buf.drain(start..end);
        self.len -= count as u32;
    }

    /// Keep only elements for which `keep(index, value)` is true. Returns how many were removed.
    pub fn retain(&mut self, mut keep: impl FnMut(usize, &[u8]) -> bool) -> usize {
        let mut out = Vec::with_capacity(self.buf.len());
        let mut kept = 0;
        for (i, v) in self.iter().enumerate() {
            if keep(i, v) {
                write_varint(&mut out, v.len());
                out.extend_from_slice(v);
                kept += 1;
            }
        }
        let removed = self.len() - kept;
        self.buf = out;
        self.len = kept as u32;
        removed
    }

    /// Index of the first element equal to `v`.
    pub fn position(&self, v: &[u8]) -> Option<usize> {
        self.iter().position(|e| e == v)
    }

    // --- pair helpers (hashes and sorted sets store field/value or member/score pairs) ---

    /// Number of pairs.
    pub fn pairs(&self) -> usize {
        self.len() / 2
    }

    pub fn iter_pairs(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        let mut it = self.iter();
        std::iter::from_fn(move || Some((it.next()?, it.next()?)))
    }

    /// Value of the pair whose first element is `k`.
    pub fn pair_get(&self, k: &[u8]) -> Option<&[u8]> {
        self.iter_pairs().find(|(f, _)| *f == k).map(|(_, v)| v)
    }

    /// Byte offset of the pair keyed `k`.
    fn pair_offset(&self, k: &[u8]) -> Option<usize> {
        let mut pos = 0;
        while pos < self.buf.len() {
            let (f, next) = self.at(pos);
            let (_, after) = self.at(next);
            if f == k {
                return Some(pos);
            }
            pos = after;
        }
        None
    }

    /// Set the value of pair `k` (appending a new pair when absent). Returns true if inserted.
    pub fn pair_set(&mut self, k: &[u8], v: &[u8]) -> bool {
        match self.pair_offset(k) {
            Some(at) => {
                let (_, vstart) = self.at(at);
                let (_, vend) = self.at(vstart);
                self.buf.splice(vstart..vend, encode(v));
                false
            }
            None => {
                self.push_back(k);
                self.push_back(v);
                true
            }
        }
    }

    /// Remove pair `k`; returns its value.
    pub fn pair_remove(&mut self, k: &[u8]) -> Option<Vec<u8>> {
        let at = self.pair_offset(k)?;
        let (_, vstart) = self.at(at);
        let (v, vend) = self.at(vstart);
        let v = v.to_vec();
        self.buf.drain(at..vend);
        self.len -= 2;
        Some(v)
    }

    /// Insert a pair before pair index `i` (keeps sorted-set order).
    pub fn pair_insert(&mut self, i: usize, k: &[u8], v: &[u8]) {
        let at = self.offset((2 * i).min(self.len()));
        let mut e = encode(k);
        e.extend_from_slice(&encode(v));
        self.buf.splice(at..at, e);
        self.len += 2;
    }

    /// Remove pair index `i`.
    pub fn pair_remove_at(&mut self, i: usize) {
        self.remove_range(2 * i, 2);
    }
}

pub struct PackIter<'a> {
    buf: &'a [u8],
    pos: usize,
    left: usize,
}

impl<'a> Iterator for PackIter<'a> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        let (n, p) = read_varint(self.buf, self.pos);
        self.pos = p + n;
        self.left -= 1;
        Some(&self.buf[p..p + n])
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for PackIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_ops() {
        let mut p = Pack::new();
        p.push_back(b"b");
        p.push_front(b"a");
        p.push_back(&[7u8; 300]);
        p.insert(2, b"c");
        assert_eq!(p.len(), 4);
        assert_eq!(p.get(0), Some(&b"a"[..]));
        assert_eq!(p.get(2), Some(&b"c"[..]));
        assert_eq!(p.last().unwrap().len(), 300);
        p.set(1, b"bbbbbb");
        assert_eq!(p.iter().collect::<Vec<_>>(), vec![&b"a"[..], b"bbbbbb", b"c", &[7u8; 300]]);
        assert_eq!(p.pop_back().unwrap().len(), 300);
        assert_eq!(p.pop_front().unwrap(), b"a");
        p.remove_range(0, 5);
        assert!(p.is_empty());
        assert_eq!(p.bytes(), 0);
    }

    #[test]
    fn pairs() {
        let mut p = Pack::new();
        assert!(p.pair_set(b"f1", b"v1"));
        assert!(p.pair_set(b"f2", b"v2"));
        assert!(!p.pair_set(b"f1", b"longer value"));
        assert_eq!(p.pair_get(b"f1"), Some(&b"longer value"[..]));
        assert_eq!(p.pairs(), 2);
        assert_eq!(p.pair_remove(b"f1").unwrap(), b"longer value");
        assert_eq!(p.iter_pairs().collect::<Vec<_>>(), vec![(&b"f2"[..], &b"v2"[..])]);
        p.pair_insert(0, b"a", b"1");
        assert_eq!(p.iter_pairs().next(), Some((&b"a"[..], &b"1"[..])));
        p.pair_remove_at(0);
        assert_eq!(p.pairs(), 1);
    }

    #[test]
    fn retain() {
        let mut p = Pack::from_items(["1", "2", "3", "4"]);
        assert_eq!(p.retain(|_, v| v != b"2" && v != b"4"), 2);
        assert_eq!(p.iter().collect::<Vec<_>>(), vec![&b"1"[..], b"3"]);
    }
}
