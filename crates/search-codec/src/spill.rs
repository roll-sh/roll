//! Spill buffers: append-only byte streams that live in memory up to a budget and continue in
//! an anonymous temporary file beyond it.
//!
//! A segment is one sequential file, but some of its sections are produced side by side: while
//! a field's terms are written, its doc stream goes straight into the container, yet the
//! positions, payloads, term index and term metadata of the same field grow at the same time.
//! Large merges would otherwise hold all of them in memory at once (gigabytes for a field of a
//! 5 GB segment). Each of those streams is a `Spill`: small fields never touch the disk, large
//! ones keep at most `memory_bytes` of each stream in memory and the rest in a temporary file
//! that is copied into the container when the section is written.
//!
//! Temporary files are created next to the segment's other local data
//! ([`SpillOptions::dir`]), unlinked right after creation on Unix (so a crash leaves nothing
//! behind) and removed on drop elsewhere.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::io::ByteSink;
use crate::error::Result;

/// Where and when section buffers spill to disk while writing a segment.
#[derive(Clone, Debug)]
pub struct SpillOptions {
    /// Directory for temporary files (`None`: [`std::env::temp_dir`]). Use a directory on the
    /// same disk as the local segment cache: `/tmp` is often memory-backed.
    pub dir: Option<PathBuf>,
    /// Bytes each spillable stream keeps in memory before continuing on disk. `usize::MAX`
    /// keeps everything in memory.
    pub memory_bytes: usize,
}

impl Default for SpillOptions {
    fn default() -> SpillOptions {
        SpillOptions { dir: None, memory_bytes: 64 << 20 }
    }
}

impl SpillOptions {
    /// Never spill.
    pub fn in_memory() -> SpillOptions {
        SpillOptions { dir: None, memory_bytes: usize::MAX }
    }
}

/// An append-only byte stream, in memory up to the budget, then in a temporary file.
pub(crate) struct Spill {
    mem: Vec<u8>,
    file: Option<TempFile>,
    on_disk: u64,
    limit: usize,
    dir: Option<PathBuf>,
}

impl Spill {
    pub(crate) fn new(opts: &SpillOptions) -> Spill {
        Spill {
            mem: Vec::new(),
            file: None,
            on_disk: 0,
            limit: opts.memory_bytes,
            dir: opts.dir.clone(),
        }
    }

    /// Bytes appended so far.
    pub(crate) fn len(&self) -> u64 {
        self.on_disk + self.mem.len() as u64
    }

    pub(crate) fn append(&mut self, b: &[u8]) -> Result<()> {
        if self.file.is_none() && self.mem.len().saturating_add(b.len()) <= self.limit {
            self.mem.extend_from_slice(b);
            return Ok(());
        }
        if self.file.is_none() {
            self.file = Some(TempFile::create(self.dir.as_ref())?);
        }
        let f = self.file.as_mut().unwrap();
        // Keep up to a small write buffer in memory so tiny appends do not each hit the file.
        self.mem.extend_from_slice(b);
        if self.mem.len() >= WRITE_CHUNK.min(self.limit.max(1)) {
            f.file.write_all(&self.mem)?;
            self.on_disk += self.mem.len() as u64;
            self.mem.clear();
            self.mem.shrink_to(WRITE_CHUNK);
        }
        Ok(())
    }

    /// Writes the whole stream into `w` (the container's open section, or another sink).
    pub(crate) fn copy_to(&mut self, w: &mut dyn ByteSink) -> Result<()> {
        self.copy_with(&mut |b| w.put_all(b))
    }

    /// Hands the whole stream to `f` in chunks.
    pub(crate) fn copy_with(&mut self, f: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if let Some(file) = &mut self.file {
            file.file.seek(SeekFrom::Start(0))?;
            let mut buf = vec![0u8; WRITE_CHUNK];
            let mut left = self.on_disk;
            while left > 0 {
                let n = (left as usize).min(buf.len());
                file.file.read_exact(&mut buf[..n])?;
                f(&buf[..n])?;
                left -= n as u64;
            }
        }
        f(&self.mem)
    }

    /// All the bytes as one mutable buffer: the in-memory vector, or the temporary file mapped
    /// into memory (page cache the OS can evict, not heap), for structures that need random
    /// access to everything they spilled (a field's raw vectors while its graph is built).
    pub(crate) fn into_buffer(mut self) -> Result<SpillBuffer> {
        let Some(mut f) = self.file.take() else {
            return Ok(SpillBuffer::Mem(std::mem::take(&mut self.mem)));
        };
        f.file.seek(SeekFrom::End(0))?;
        f.file.write_all(&self.mem)?;
        let len = self.on_disk + self.mem.len() as u64;
        if len == 0 {
            return Ok(SpillBuffer::Mem(Vec::new()));
        }
        f.file.flush()?;
        // SAFETY: the file is private to this process (created exclusively and unlinked or
        // removed on drop) and only accessed through this mapping from now on.
        let map = unsafe { memmap2::MmapOptions::new().len(len as usize).map_mut(&f.file)? };
        Ok(SpillBuffer::Map(map, f))
    }

    /// The bytes, when nothing spilled (tests and small fields).
    #[cfg(test)]
    pub(crate) fn in_memory(&self) -> Option<&[u8]> {
        self.file.is_none().then_some(&self.mem[..])
    }

    /// Whether part of the stream is on disk.
    #[cfg(test)]
    pub(crate) fn spilled(&self) -> bool {
        self.file.is_some()
    }
}

impl ByteSink for Spill {
    fn put_all(&mut self, b: &[u8]) -> Result<()> {
        self.append(b)
    }
}

/// Element types of a [`SpillVec`].
///
/// # Safety
/// Implementors have no padding and every bit pattern is a valid value, so a slice of them can
/// be viewed as bytes and a byte buffer (of the right alignment) as a slice of them.
pub(crate) unsafe trait Pod: Copy + Default + Send + Sync + 'static {}
// SAFETY: primitive integers have no padding and no invalid bit patterns.
unsafe impl Pod for u8 {}
// SAFETY: as above.
unsafe impl Pod for u32 {}
// SAFETY: as above.
unsafe impl Pod for u64 {}
// SAFETY: as above.
unsafe impl Pod for i64 {}
// SAFETY: as above.
unsafe impl Pod for u128 {}

fn pod_bytes<T: Pod>(s: &[T]) -> &[u8] {
    // SAFETY: `T: Pod` has no padding, so every byte of the slice is initialized.
    unsafe { std::slice::from_raw_parts(s.as_ptr().cast::<u8>(), std::mem::size_of_val(s)) }
}

/// A typed, append-only array that keeps up to the spill budget in memory and continues in a
/// temporary file, then is read back as one mutable slice ([`SpillSlice`]): the per-value
/// scratch arrays of the doc values and points writers (doc ids, values, ordinals, addresses,
/// sort permutations). A large field's arrays then live in page cache the OS can write back
/// and evict instead of in heap, so merging tens of millions of values cannot run the node out
/// of memory.
pub(crate) struct SpillVec<T: Pod> {
    mem: Vec<T>,
    file: Option<TempFile>,
    on_disk: usize,
    /// `mem.len()` at which `push` takes the slow path (the budget, then the write chunk).
    flush_at: usize,
    dir: Option<PathBuf>,
}

impl<T: Pod> SpillVec<T> {
    pub(crate) fn new(opts: &SpillOptions) -> SpillVec<T> {
        SpillVec {
            mem: Vec::new(),
            file: None,
            on_disk: 0,
            flush_at: (opts.memory_bytes / std::mem::size_of::<T>()).max(1),
            dir: opts.dir.clone(),
        }
    }

    /// Elements pushed so far.
    pub(crate) fn len(&self) -> usize {
        self.on_disk + self.mem.len()
    }

    #[inline]
    pub(crate) fn push(&mut self, v: T) -> Result<()> {
        self.mem.push(v);
        if self.mem.len() >= self.flush_at {
            self.flush()?;
        }
        Ok(())
    }

    pub(crate) fn extend_from_slice(&mut self, s: &[T]) -> Result<()> {
        self.mem.extend_from_slice(s);
        if self.mem.len() >= self.flush_at {
            self.flush()?;
        }
        Ok(())
    }

    #[cold]
    fn flush(&mut self) -> Result<()> {
        if self.file.is_none() {
            self.file = Some(TempFile::create(self.dir.as_ref())?);
        }
        let f = self.file.as_mut().unwrap();
        f.file.write_all(pod_bytes(&self.mem))?;
        self.on_disk += self.mem.len();
        self.mem.clear();
        let chunk = (WRITE_CHUNK / std::mem::size_of::<T>()).max(1);
        self.mem.shrink_to(chunk);
        self.flush_at = chunk;
        Ok(())
    }

    /// Every element as one mutable slice: the vector itself, or the file mapped back.
    pub(crate) fn finish(mut self) -> Result<SpillSlice<T>> {
        let Some(mut f) = self.file.take() else {
            return Ok(SpillSlice::Mem(std::mem::take(&mut self.mem)));
        };
        f.file.write_all(pod_bytes(&self.mem))?;
        let len = self.on_disk + self.mem.len();
        if len == 0 {
            return Ok(SpillSlice::Mem(Vec::new()));
        }
        f.file.flush()?;
        // SAFETY: the file is private to this process (created exclusively, unlinked or removed
        // on drop) and only accessed through this mapping from now on.
        let map = unsafe {
            memmap2::MmapOptions::new().len(len * std::mem::size_of::<T>()).map_mut(&f.file)?
        };
        Ok(SpillSlice::Map { map, _file: f, len })
    }
}

/// The contents of a finished [`SpillVec`].
pub(crate) enum SpillSlice<T: Pod> {
    Mem(Vec<T>),
    /// A page-aligned mapping of `len` elements and the file it maps.
    Map { map: memmap2::MmapMut, _file: TempFile, len: usize },
}

impl<T: Pod> SpillSlice<T> {
    /// `len` default (zero) elements: in memory within the budget, else a sparse temporary file
    /// mapped (nothing is written until the pages are touched).
    pub(crate) fn zeroed(len: usize, opts: &SpillOptions) -> Result<SpillSlice<T>> {
        let bytes = len.saturating_mul(std::mem::size_of::<T>());
        if bytes <= opts.memory_bytes || len == 0 {
            return Ok(SpillSlice::Mem(vec![T::default(); len]));
        }
        let f = TempFile::create(opts.dir.as_ref())?;
        f.file.set_len(bytes as u64)?;
        // SAFETY: as in `SpillVec::finish`.
        let map = unsafe { memmap2::MmapOptions::new().len(bytes).map_mut(&f.file)? };
        Ok(SpillSlice::Map { map, _file: f, len })
    }

    /// Whether the elements live in a mapped file.
    #[cfg(test)]
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self, SpillSlice::Map { .. })
    }
}

impl<T: Pod> std::ops::Deref for SpillSlice<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            SpillSlice::Mem(v) => v,
            // SAFETY: the mapping is page aligned (so aligned for `T`), holds exactly `len`
            // elements written from `T` values, and `T: Pod` accepts any bit pattern.
            SpillSlice::Map { map, len, .. } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().cast::<T>(), *len)
            },
        }
    }
}

impl<T: Pod> std::ops::DerefMut for SpillSlice<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            SpillSlice::Mem(v) => v,
            // SAFETY: as in `deref`; the mapping is writable and exclusively ours.
            SpillSlice::Map { map, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(map.as_mut_ptr().cast::<T>(), *len)
            },
        }
    }
}

impl Write for Spill {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.append(buf).map_err(std::io::Error::from)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const WRITE_CHUNK: usize = 1 << 20;

/// See [`Spill::into_buffer`].
pub(crate) enum SpillBuffer {
    Mem(Vec<u8>),
    /// The mapping, and the file it maps (kept open while mapped).
    Map(memmap2::MmapMut, #[allow(dead_code)] TempFile),
}

impl std::ops::Deref for SpillBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            SpillBuffer::Mem(v) => v,
            SpillBuffer::Map(m, _) => m,
        }
    }
}

impl std::ops::DerefMut for SpillBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            SpillBuffer::Mem(v) => v,
            SpillBuffer::Map(m, _) => m,
        }
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A temporary file, unlinked at creation where the OS allows it.
pub(crate) struct TempFile {
    file: File,
    /// Still to be removed on drop (non-Unix).
    path: Option<PathBuf>,
}

impl TempFile {
    fn create(dir: Option<&PathBuf>) -> Result<TempFile> {
        let dir = dir.cloned().unwrap_or_else(std::env::temp_dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        loop {
            let name = format!(
                ".roll-codec-{}-{}-{nanos:08x}.spill",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = dir.join(name);
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        std::fs::remove_file(&path)?;
                        return Ok(TempFile { file, path: None });
                    }
                    #[cfg(not(unix))]
                    return Ok(TempFile { file, path: Some(path) });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{ContainerReader, ContainerWriter, SectionKind};
    use crate::slice::Slice;

    fn round_trip(limit: usize, parts: &[Vec<u8>]) -> (Vec<u8>, bool) {
        let dir = std::env::temp_dir();
        let mut s = Spill::new(&SpillOptions { dir: Some(dir), memory_bytes: limit });
        let mut want = Vec::new();
        for p in parts {
            s.append(p).unwrap();
            want.extend_from_slice(p);
            assert_eq!(s.len(), want.len() as u64);
        }
        let spilled = s.spilled();
        let mut out = Vec::new();
        let mut w = ContainerWriter::new(&mut out, [1; 16]).unwrap();
        w.begin(SectionKind::Positions, 3).unwrap();
        s.copy_to(&mut w).unwrap();
        w.end().unwrap();
        w.finish().unwrap();
        let r = ContainerReader::open(Slice::from(out)).unwrap();
        r.verify().unwrap();
        let got = r.section(SectionKind::Positions, 3).unwrap().to_vec();
        assert_eq!(got, want);
        (got, spilled)
    }

    #[test]
    fn spills_beyond_budget_and_copies_back() {
        let parts: Vec<Vec<u8>> = (0..300u32)
            .map(|i| (0..(i * 37 % 9000)).map(|j| (i ^ j) as u8).collect())
            .collect();
        let (_, spilled) = round_trip(usize::MAX, &parts);
        assert!(!spilled);
        let (_, spilled) = round_trip(10_000, &parts);
        assert!(spilled);
        let (_, spilled) = round_trip(0, &parts);
        assert!(spilled);
        let (got, _) = round_trip(0, &[]);
        assert!(got.is_empty());
        // Larger than the copy chunk.
        let big = vec![(0..3 * WRITE_CHUNK + 5).map(|i| (i % 251) as u8).collect::<Vec<u8>>()];
        let (_, spilled) = round_trip(1000, &big);
        assert!(spilled);
    }

    #[test]
    fn spilled_buffers_map_back() {
        for limit in [usize::MAX, 0, 100, 1 << 20] {
            let mut s = Spill::new(&SpillOptions { dir: None, memory_bytes: limit });
            let want: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
            for c in want.chunks(777) {
                s.append(c).unwrap();
            }
            let mut b = s.into_buffer().unwrap();
            assert_eq!(&b[..], &want[..], "limit {limit}");
            assert_eq!(matches!(b, SpillBuffer::Map(..)), limit < want.len());
            b[5] = 99;
            assert_eq!(b[5], 99);
        }
        let s = Spill::new(&SpillOptions { dir: None, memory_bytes: 0 });
        assert!(s.into_buffer().unwrap().is_empty());
    }

    #[test]
    fn zeroed_slices() {
        for limit in [0usize, 100, usize::MAX] {
            let opts = SpillOptions { dir: None, memory_bytes: limit };
            let mut z = SpillSlice::<u64>::zeroed(10_000, &opts).unwrap();
            assert_eq!(z.is_mapped(), limit < 80_000);
            assert!(z.iter().all(|&x| x == 0));
            z[9_999] = 7;
            assert_eq!(z[9_999], 7);
            assert!(SpillSlice::<u128>::zeroed(0, &opts).unwrap().is_empty());
        }
    }

    #[test]
    fn spill_vecs_round_trip_and_sort_in_place() {
        for limit in [usize::MAX, 0, 1, 7, 4096, 1 << 20] {
            let opts = SpillOptions { dir: None, memory_bytes: limit };
            let mut v = SpillVec::<u64>::new(&opts);
            let want: Vec<u64> = (0..200_000u64).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 7).collect();
            for (i, c) in want.chunks(333).enumerate() {
                if i % 2 == 0 {
                    v.extend_from_slice(c).unwrap();
                } else {
                    for &x in c {
                        v.push(x).unwrap();
                    }
                }
            }
            assert_eq!(v.len(), want.len());
            let mut s = v.finish().unwrap();
            assert_eq!(s.is_mapped(), limit < want.len() * 8, "limit {limit}");
            assert_eq!(&s[..], &want[..]);
            s.sort_unstable();
            let mut sorted = want.clone();
            sorted.sort_unstable();
            assert_eq!(&s[..], &sorted[..]);
            // Byte elements and an empty vector.
            let mut b = SpillVec::<u8>::new(&opts);
            b.extend_from_slice(&[1, 2, 3]).unwrap();
            assert_eq!(&b.finish().unwrap()[..], &[1, 2, 3]);
            assert!(SpillVec::<i64>::new(&opts).finish().unwrap().is_empty());
        }
    }

    #[test]
    fn temp_files_leave_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("roll-codec-spill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut s = Spill::new(&SpillOptions { dir: Some(dir.clone()), memory_bytes: 0 });
            s.append(&[1, 2, 3]).unwrap();
            assert!(s.spilled());
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir(&dir).unwrap();
        // A missing directory is an I/O error, not a panic.
        let mut s = Spill::new(&SpillOptions { dir: Some(dir.join("missing")), memory_bytes: 0 });
        assert!(s.append(&[1]).is_err());
    }
}
