//! `Slice`: a cheap-clone, immutable view of owned bytes.
//!
//! Every reader in the codec works over a `Slice`. It is a thin wrapper over [`bytes::Bytes`], so
//! the backing storage can be a `Vec<u8>`, a `Bytes` received from object storage, or a memory
//! map (`memmap2::Mmap`) kept alive by the slice's reference count. Sub-slicing and cloning only
//! bump a reference count: readers keep sub-slices of the sections they use and never copy whole
//! sections.

use std::fmt;
use std::ops::{Deref, Range};
use std::path::Path;

use bytes::Bytes;

/// An immutable, reference-counted byte view.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Slice(Bytes);

impl Slice {
    /// An empty slice.
    pub const fn empty() -> Slice {
        Slice(Bytes::new())
    }

    /// Wraps any owner of bytes (a `Vec<u8>`, a `memmap2::Mmap`, ...) without copying.
    pub fn from_owner<T>(owner: T) -> Slice
    where
        T: AsRef<[u8]> + Send + 'static,
    {
        Slice(Bytes::from_owner(owner))
    }

    /// Memory-maps a file read-only.
    ///
    /// The file must not be modified while mapped; segment files are immutable by design.
    pub fn map_file(path: impl AsRef<Path>) -> std::io::Result<Slice> {
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Ok(Slice::empty());
        }
        // SAFETY: segment files are written once and never modified in place; the cache only
        // ever replaces them atomically (rename), which leaves existing mappings intact.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Slice::from_owner(map))
    }

    /// The bytes.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// A sub-view sharing the same storage.
    ///
    /// # Panics
    /// If the range is out of bounds.
    #[inline]
    pub fn slice(&self, range: Range<usize>) -> Slice {
        Slice(self.0.slice(range))
    }

    /// A checked sub-view.
    #[inline]
    pub fn try_slice(&self, start: u64, len: u64) -> Option<Slice> {
        let end = start.checked_add(len)?;
        if end > self.0.len() as u64 {
            return None;
        }
        Some(self.slice(start as usize..end as usize))
    }

    /// Whether both views cover the same bytes of the same storage (no content comparison).
    #[inline]
    pub fn same_view(&self, other: &Slice) -> bool {
        self.0.as_ptr() == other.0.as_ptr() && self.0.len() == other.0.len()
    }

    /// The underlying `Bytes`.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

impl Deref for Slice {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for Slice {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Slice {
    fn from(v: Vec<u8>) -> Slice {
        Slice(Bytes::from(v))
    }
}

impl From<Bytes> for Slice {
    fn from(b: Bytes) -> Slice {
        Slice(b)
    }
}

impl From<&'static [u8]> for Slice {
    fn from(b: &'static [u8]) -> Slice {
        Slice(Bytes::from_static(b))
    }
}

impl fmt::Debug for Slice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slice({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_slices_share_storage() {
        let s = Slice::from(vec![1u8, 2, 3, 4, 5]);
        let t = s.slice(1..4);
        assert_eq!(&*t, &[2, 3, 4]);
        assert_eq!(t.as_ptr(), s[1..].as_ptr());
        assert!(s.try_slice(3, 3).is_none());
        assert_eq!(&*s.try_slice(3, 2).unwrap(), &[4, 5]);
        assert!(s.try_slice(u64::MAX, 2).is_none());
    }

    #[test]
    fn maps_files() {
        let dir = std::env::temp_dir().join(format!("roll-codec-slice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.bin");
        std::fs::write(&p, b"hello").unwrap();
        let s = Slice::map_file(&p).unwrap();
        assert_eq!(&*s, b"hello");
        let e = dir.join("empty.bin");
        std::fs::write(&e, b"").unwrap();
        assert!(Slice::map_file(&e).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
