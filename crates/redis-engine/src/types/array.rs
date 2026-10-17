//! The `array` type (Redis 8.10 sparse arrays): a random-access sequence indexed by any `u64`
//! below `u64::MAX`, with holes.
//!
//! The layout follows Redis' so `ARINFO` reports the same numbers: a directory of fixed-size
//! slices (`array-slice-size`, a power of two). A slice is either *sparse* (sorted offsets and
//! values, at most `array-sparse-kmax` of them) or *dense* (a power-of-two window of slots that
//! slides and grows inside the slice). Dense slices that drop to `array-sparse-kmin` items are
//! demoted back when that saves memory. The directory is flat (a power-of-two vector of slice
//! pointers) until a slice id reaches 2048; then it becomes a *superdir*: sorted blocks of 2048
//! slice pointers allocated on demand, so huge indexes don't need a huge directory.
//!
//! Values are one tagged word each, like Redis: an integer that fits 62 bits, a double whose low
//! two mantissa bits are zero (only when its text round-trips), a string of up to 7 bytes, or a
//! pointer to a heap string. What a client reads back is always the original text.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::num::NonZeroU64;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("array values are tagged 64-bit words");

pub const SLICE_SIZE_DEFAULT: u32 = 4096;
pub const SLICE_SIZE_MIN: u32 = 256;
pub const SLICE_SIZE_MAX: u32 = 65536;
pub const KMAX_DEFAULT: u32 = 10;
pub const KMIN_DEFAULT: u32 = 5;
/// No ARINSERT yet (or ARSEEK 0): the next insert goes to index 0.
pub const INSERT_NONE: u64 = u64::MAX;

const SUPER_BLOCK_SLOTS: u64 = 2048;
const SLICE_MIN_ALLOC: u32 = 8;

const TAG_PTR: u64 = 0;
const TAG_INT: u64 = 1;
const TAG_FLOAT: u64 = 2;
const TAG_STR: u64 = 3;
const TAG_MASK: u64 = 3;
const SMALLSTR_MAX: usize = 7;
const INT_MIN: i64 = -(1 << 61);
const INT_MAX: i64 = (1 << 61) - 1;

/// RDB element tags.
pub const RDB_TAG_SDS: u64 = 0;
pub const RDB_TAG_INT: u64 = 1;
pub const RDB_TAG_FLOAT: u64 = 2;
pub const RDB_TAG_SMALLSTR: u64 = 3;

/// Sparse/dense thresholds, read at operation time like Redis' globals.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub kmax: u32,
    pub kmin: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { kmax: KMAX_DEFAULT, kmin: KMIN_DEFAULT }
    }
}

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

/// One stored element (a tagged word; heap strings are `[len: u64][bytes]` allocations).
#[repr(transparent)]
pub struct Val(NonZeroU64);

/// What a value is, for RDB and AROP.
pub enum Decoded<'a> {
    Int(i64),
    Float(f64),
    Small(&'a [u8]),
    Heap(&'a [u8]),
}

impl Val {
    fn word(&self) -> u64 {
        self.0.get()
    }

    fn tag(&self) -> u64 {
        self.word() & TAG_MASK
    }

    fn from_word(w: u64) -> Val {
        Val(NonZeroU64::new(w).expect("tagged words are never zero"))
    }

    pub fn int(i: i64) -> Option<Val> {
        (INT_MIN..=INT_MAX).contains(&i).then(|| Val::from_word(((i as u64) << 2) | TAG_INT))
    }

    /// A double with the low two bits of its representation cleared.
    pub fn float_bits(bits: u64) -> Val {
        Val::from_word((bits & !TAG_MASK) | TAG_FLOAT)
    }

    pub fn small(s: &[u8]) -> Val {
        debug_assert!(s.len() <= SMALLSTR_MAX);
        let mut w = TAG_STR | ((s.len() as u64) << 2);
        for (i, &c) in s.iter().enumerate() {
            w |= (c as u64) << (8 * (i + 1));
        }
        Val::from_word(w)
    }

    fn heap_layout(len: usize) -> Layout {
        Layout::from_size_align(8 + len, 8).expect("string too large")
    }

    pub fn heap(s: &[u8]) -> Val {
        let layout = Self::heap_layout(s.len());
        // SAFETY: the layout is non-zero sized and 8-aligned (so the low tag bits are 00); we
        // write the length header and copy the bytes into the fresh allocation.
        unsafe {
            let p = alloc(layout);
            if p.is_null() {
                handle_alloc_error(layout);
            }
            (p as *mut u64).write(s.len() as u64);
            std::ptr::copy_nonoverlapping(s.as_ptr(), p.add(8), s.len());
            Val::from_word(p as u64)
        }
    }

    fn heap_bytes(&self) -> &[u8] {
        let p = self.word() as *const u8;
        // SAFETY: a TAG_PTR word is a live allocation made by `heap`, owned by `self`.
        unsafe {
            let len = (p as *const u64).read() as usize;
            std::slice::from_raw_parts(p.add(8), len)
        }
    }

    pub fn is_heap(&self) -> bool {
        self.tag() == TAG_PTR
    }

    /// `arEncode`: integer, round-tripping float, small string or heap string.
    pub fn encode(s: &[u8]) -> Val {
        if let Some(v) = crate::util::parse_i64(s).and_then(Val::int) {
            return v;
        }
        if let Some(v) = try_float(s) {
            return v;
        }
        if s.len() <= SMALLSTR_MAX { Val::small(s) } else { Val::heap(s) }
    }

    pub fn decoded(&self) -> Decoded<'_> {
        match self.tag() {
            TAG_INT => Decoded::Int((self.word() as i64) >> 2),
            TAG_FLOAT => Decoded::Float(f64::from_bits(self.word() & !TAG_MASK)),
            TAG_STR => {
                let len = ((self.word() >> 2) & 7) as usize;
                // Little-endian: byte i+1 of the word is character i.
                // SAFETY-free trick: borrow the word's own bytes.
                let bytes: &[u8; 8] = bytemuck_word(&self.0);
                Decoded::Small(&bytes[1..1 + len])
            }
            _ => Decoded::Heap(self.heap_bytes()),
        }
    }

    /// The value's text (`arDecode`).
    pub fn text(&self) -> std::borrow::Cow<'_, [u8]> {
        use std::borrow::Cow;
        match self.decoded() {
            Decoded::Int(i) => Cow::Owned(i.to_string().into_bytes()),
            Decoded::Float(d) => Cow::Owned(format_float(d).into_bytes()),
            Decoded::Small(s) | Decoded::Heap(s) => Cow::Borrowed(s),
        }
    }

    /// Bytes owned outside the slot, as Redis allocates them (`arStringNew`: a 2 byte length
    /// header, 8 past 32767 bytes).
    fn heap_mem(&self) -> usize {
        if !self.is_heap() {
            return 0;
        }
        let len = self.heap_bytes().len();
        usable(if len <= 32767 { 2 } else { 8 } + len)
    }
}

/// The word's bytes in memory order (little-endian targets only, checked below).
fn bytemuck_word(w: &NonZeroU64) -> &[u8; 8] {
    // SAFETY: NonZeroU64 is 8 bytes with no padding; any byte pattern is a valid [u8; 8].
    unsafe { &*(w as *const NonZeroU64 as *const [u8; 8]) }
}

#[cfg(not(target_endian = "little"))]
compile_error!("small array strings assume a little-endian word layout");

impl Drop for Val {
    fn drop(&mut self) {
        if self.is_heap() {
            let len = self.heap_bytes().len();
            // SAFETY: allocated by `heap` with this exact layout and not freed before.
            unsafe { dealloc(self.word() as *mut u8, Self::heap_layout(len)) }
        }
    }
}

impl Clone for Val {
    fn clone(&self) -> Self {
        if self.is_heap() { Val::heap(self.heap_bytes()) } else { Val(self.0) }
    }
}

impl std::fmt::Debug for Val {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(&self.text()))
    }
}

/// `arTryEncodeFloat`: `-?[0-9.]+` with exactly one dot whose double, with the low two bits
/// cleared, prints back as the same text (or the same text plus `.0`).
fn try_float(s: &[u8]) -> Option<Val> {
    let body = s.strip_prefix(b"-").unwrap_or(s);
    if body.is_empty() || body.iter().filter(|&&c| c == b'.').count() != 1 || !body.iter().all(|&c| c == b'.' || c.is_ascii_digit()) {
        return None;
    }
    let d = crate::commands::zsets::zfloat(s)?;
    if !d.is_finite() {
        return None;
    }
    let bits = d.to_bits() & !TAG_MASK;
    let text = redis_proto::fmt_double(f64::from_bits(bits));
    if text.as_bytes() == s || (s.len() == text.len() + 2 && s.starts_with(text.as_bytes()) && s.ends_with(b".0")) {
        return Some(Val::float_bits(bits));
    }
    None
}

/// `arFormatFloat`: `d2string`, with `.0` restored on integral-looking finite values.
pub fn format_float(d: f64) -> String {
    let mut s = redis_proto::fmt_double(d);
    if d.is_finite() && !s.contains(['.', 'e', 'E']) {
        s.push_str(".0");
    }
    s
}

/// `arValueFromRdbFloat` (64-bit: the stored double is used as is).
pub fn val_from_rdb_float(d: f64) -> Val {
    Val::float_bits(d.to_bits())
}

/// `arValueFromRdbInt`.
pub fn val_from_rdb_int(i: i64) -> Val {
    Val::int(i).unwrap_or_else(|| Val::heap(i.to_string().as_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Slices
// ---------------------------------------------------------------------------------------------

/// `sizeof(arSlice)` in Redis (for the demotion heuristic and memory figures).
const SLICE_HDR: usize = 32;

/// What the allocator gives for `n` bytes (nothing for none).
fn usable(n: usize) -> usize {
    if n == 0 { 0 } else { crate::client::jemalloc_size(n) }
}

/// `zmalloc_size(ar)`: the array header.
const ARRAY_HDR: usize = 80;

fn dense_alloc_size(winsize: u32) -> usize {
    SLICE_HDR + winsize as usize * 8
}

fn sparse_alloc_size(cap: u32) -> usize {
    let offsets = cap as usize * 2;
    let pad = (8 - offsets % 8) % 8;
    SLICE_HDR + offsets + pad + cap as usize * 8
}

/// `nearestNextPowerOf2` (unsigned int).
fn next_pow2(n: u32) -> u32 {
    if n <= 1 { 1 } else { 1 << (32 - (n - 1).leading_zeros()) }
}

#[derive(Clone, Debug)]
struct Dense {
    count: u32,
    /// First slice offset covered by the window.
    offset: u32,
    /// Highest offset holding a value.
    max_idx: u32,
    /// The window (`winsize` slots).
    items: Box<[Option<Val>]>,
}

#[derive(Clone, Debug)]
struct Sparse {
    cap: u32,
    offsets: Vec<u16>,
    values: Vec<Val>,
}

#[derive(Clone, Debug)]
enum Slice {
    Dense(Dense),
    Sparse(Sparse),
}

impl Dense {
    fn winsize(&self) -> u32 {
        self.items.len() as u32
    }

    /// `arSliceDenseNew`.
    fn new(rel: u32, slice_size: u32) -> Dense {
        let winsize = SLICE_MIN_ALLOC;
        let offset = if rel + winsize > slice_size { slice_size - winsize } else { rel };
        Dense { count: 0, offset, max_idx: 0, items: empty_window(winsize) }
    }

    fn get(&self, rel: u32) -> Option<&Val> {
        if rel < self.offset || rel >= self.offset + self.winsize() {
            return None;
        }
        self.items[(rel - self.offset) as usize].as_ref()
    }

    /// Re-window: the old items land at `new_offset`-relative positions.
    fn rewindow(&mut self, new_offset: u32, new_winsize: u32) {
        let mut items = empty_window(new_winsize);
        let shift = (self.offset - new_offset) as usize;
        for (i, v) in std::mem::take(&mut self.items).into_vec().into_iter().enumerate() {
            items[shift + i] = v;
        }
        self.items = items;
        self.offset = new_offset;
    }

    /// `arSliceDenseGrowRight`.
    fn grow_right(&mut self, rel: u32, slice_size: u32) {
        let mut winsize = self.winsize();
        while rel >= self.offset + winsize && winsize < slice_size {
            winsize <<= 1;
        }
        let mut offset = self.offset;
        if winsize >= slice_size {
            winsize = slice_size;
            offset = 0;
        } else if offset + winsize > slice_size {
            offset = slice_size - winsize;
        }
        self.rewindow(offset, winsize);
    }

    /// `arSliceDenseGrowLeft`.
    fn grow_left(&mut self, rel: u32, slice_size: u32) {
        let end = self.offset + self.winsize();
        let winsize = next_pow2(end - rel).max(SLICE_MIN_ALLOC).min(slice_size);
        let offset = if winsize == slice_size { 0 } else { end.saturating_sub(winsize) };
        self.rewindow(offset, winsize);
    }

    fn grow_if_needed(&mut self, rel: u32, slice_size: u32) {
        if rel >= self.offset + self.winsize() {
            self.grow_right(rel, slice_size);
        } else if rel < self.offset {
            self.grow_left(rel, slice_size);
        }
    }

    /// `arDenseUpdateMaxIdx` after deleting `deleted`.
    fn update_max_after_delete(&mut self, deleted: u32) {
        if self.count == 0 || deleted < self.max_idx {
            return;
        }
        let mut pos = (self.max_idx - self.offset) as i64;
        while pos >= 0 {
            if self.items[pos as usize].is_some() {
                self.max_idx = self.offset + pos as u32;
                return;
            }
            pos -= 1;
        }
    }
}

fn empty_window(n: u32) -> Box<[Option<Val>]> {
    (0..n).map(|_| None).collect()
}

impl Sparse {
    /// `arSliceSparseNew`.
    fn new(lim: Limits) -> Sparse {
        let cap = lim.kmax.min(4);
        Sparse { cap, offsets: Vec::with_capacity(cap as usize), values: Vec::with_capacity(cap as usize) }
    }

    fn count(&self) -> u32 {
        self.offsets.len() as u32
    }

    /// `arSparseFindPos`: lower bound and whether it's an exact hit.
    fn find(&self, rel: u16) -> (usize, bool) {
        let pos = self.offsets.partition_point(|&o| o < rel);
        (pos, self.offsets.get(pos) == Some(&rel))
    }

    /// `arSparsePromote`.
    fn promote(self, slice_size: u32) -> Dense {
        if self.offsets.is_empty() {
            return Dense::new(0, slice_size);
        }
        let min = self.offsets[0] as u32;
        let max = *self.offsets.last().unwrap() as u32;
        let mut winsize = next_pow2(max - min + 1).max(SLICE_MIN_ALLOC);
        let mut offset = min;
        if winsize >= slice_size {
            winsize = slice_size;
            offset = 0;
        } else if offset + winsize > slice_size {
            offset = slice_size - winsize;
        }
        let mut items = empty_window(winsize);
        let count = self.count();
        for (o, v) in self.offsets.into_iter().zip(self.values) {
            items[(o as u32 - offset) as usize] = Some(v);
        }
        Dense { count, offset, max_idx: max, items }
    }
}

impl Slice {
    fn count(&self) -> u32 {
        match self {
            Slice::Dense(d) => d.count,
            Slice::Sparse(s) => s.count(),
        }
    }

    /// `arSliceMaxIdx`.
    fn max_idx(&self) -> u32 {
        match self {
            Slice::Dense(d) => d.max_idx,
            Slice::Sparse(s) => *s.offsets.last().expect("non-empty slice") as u32,
        }
    }

    fn get(&self, rel: u32) -> Option<&Val> {
        match self {
            Slice::Dense(d) => d.get(rel),
            Slice::Sparse(s) => {
                let (pos, found) = s.find(rel as u16);
                found.then(|| &s.values[pos])
            }
        }
    }

    /// The slice's allocation (`arAllocAndTrack` counts what the allocator gives).
    fn alloc_size(&self) -> usize {
        usable(match self {
            Slice::Dense(d) => dense_alloc_size(d.winsize()),
            Slice::Sparse(s) => sparse_alloc_size(s.cap),
        })
    }

    /// `arDenseMaybeDemote` (a no-op on sparse slices).
    fn maybe_demote(&mut self, lim: Limits) {
        let Slice::Dense(d) = self else { return };
        if lim.kmax == 0 || d.count > lim.kmin || d.count > lim.kmax || d.winsize() == SLICE_MIN_ALLOC {
            return;
        }
        if d.winsize() < 4 * lim.kmin || dense_alloc_size(d.winsize()) < sparse_alloc_size(lim.kmin) * 5 / 4 {
            return;
        }
        let mut s = Sparse { cap: lim.kmin, offsets: Vec::with_capacity(lim.kmin as usize), values: Vec::with_capacity(lim.kmin as usize) };
        let offset = d.offset;
        for (i, v) in std::mem::take(&mut d.items).into_vec().into_iter().enumerate() {
            if let Some(v) = v {
                s.offsets.push((offset + i as u32) as u16);
                s.values.push(v);
            }
        }
        *self = Slice::Sparse(s);
    }

    /// Items in `[lo, hi]` (slice offsets), ascending or descending.
    fn items(&self, lo: u32, hi: u32, rev: bool) -> SliceIter<'_> {
        match self {
            Slice::Dense(d) => {
                let (wlo, whi) = (d.offset, d.offset + d.winsize() - 1);
                let (lo, hi) = (lo.max(wlo), hi.min(whi));
                if lo > hi {
                    return SliceIter::Empty;
                }
                SliceIter::Dense { d, lo: lo as i64, hi: hi as i64, rev }
            }
            Slice::Sparse(s) => {
                let start = s.offsets.partition_point(|&o| (o as u32) < lo);
                let end = s.offsets.partition_point(|&o| (o as u32) <= hi);
                SliceIter::Sparse { s, lo: start, hi: end, rev }
            }
        }
    }
}

enum SliceIter<'a> {
    Empty,
    Dense { d: &'a Dense, lo: i64, hi: i64, rev: bool },
    Sparse { s: &'a Sparse, lo: usize, hi: usize, rev: bool },
}

impl<'a> Iterator for SliceIter<'a> {
    type Item = (u32, &'a Val);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            SliceIter::Empty => None,
            SliceIter::Dense { d, lo, hi, rev } => {
                while lo <= hi {
                    let off = if *rev {
                        *hi -= 1;
                        *hi + 1
                    } else {
                        *lo += 1;
                        *lo - 1
                    };
                    if let Some(v) = &d.items[(off as u32 - d.offset) as usize] {
                        return Some((off as u32, v));
                    }
                }
                None
            }
            SliceIter::Sparse { s, lo, hi, rev } => {
                if lo >= hi {
                    return None;
                }
                let pos = if *rev {
                    *hi -= 1;
                    *hi
                } else {
                    *lo += 1;
                    *lo - 1
                };
                Some((s.offsets[pos] as u32, &s.values[pos]))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Directory
// ---------------------------------------------------------------------------------------------

type SlicePtr = Option<Box<Slice>>;

#[derive(Clone, Debug)]
struct Block {
    id: u64,
    count: u32,
    slots: Box<[SlicePtr]>,
}

impl Block {
    fn new(id: u64) -> Block {
        Block { id, count: 0, slots: (0..SUPER_BLOCK_SLOTS).map(|_| None).collect() }
    }
}

#[derive(Clone, Debug)]
enum Dir {
    /// `dir_alloc` = len (a power of two, or 0).
    Flat(Vec<SlicePtr>),
    /// Sorted by block id; `cap` mirrors Redis' `sdir_cap` (ARINFO directory-size).
    Super { blocks: Vec<Block>, cap: u32 },
}

const SDIR_ENTRY: usize = 24;

#[derive(Clone, Debug)]
pub struct Array {
    count: u64,
    insert_idx: u64,
    dir: Dir,
    dir_highest_used: u64,
    num_slices: u64,
    slice_size: u32,
    bits: u32,
    /// Bytes of slices, directory and heap strings.
    alloc: usize,
}

/// `ARINFO` figures.
pub struct Info {
    pub count: u64,
    pub len: u64,
    pub insert_idx: u64,
    pub slices: u64,
    pub directory_size: u64,
    pub super_dir_entries: u64,
    pub slice_size: u32,
    pub dense_slices: u64,
    pub sparse_slices: u64,
    pub dense_total_winsize: u64,
    pub dense_total_count: u64,
    pub sparse_total_cap: u64,
}

impl Default for Array {
    fn default() -> Self {
        Array::new(SLICE_SIZE_DEFAULT)
    }
}

impl Array {
    /// `arNew` with the current `array-slice-size`.
    pub fn new(slice_size: u32) -> Array {
        debug_assert!(slice_size.is_power_of_two());
        Array {
            count: 0,
            insert_idx: INSERT_NONE,
            dir: Dir::Flat(Vec::new()),
            dir_highest_used: 0,
            num_slices: 0,
            slice_size,
            bits: slice_size.trailing_zeros(),
            alloc: ARRAY_HDR,
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn slice_size(&self) -> u32 {
        self.slice_size
    }

    pub fn insert_idx(&self) -> u64 {
        self.insert_idx
    }

    pub fn set_insert_idx(&mut self, i: u64) {
        self.insert_idx = i;
    }

    pub fn mem(&self) -> usize {
        self.alloc
    }

    fn split(&self, idx: u64) -> (u64, u32) {
        (idx >> self.bits, (idx & (self.slice_size as u64 - 1)) as u32)
    }

    fn make_idx(&self, slice_id: u64, off: u32) -> u64 {
        (slice_id << self.bits) | off as u64
    }

    /// `arLen`: highest index + 1.
    pub fn len(&self) -> u64 {
        if self.count == 0 {
            return 0;
        }
        match self.slice(self.dir_highest_used) {
            Some(s) => self.make_idx(self.dir_highest_used, s.max_idx()) + 1,
            None => 0,
        }
    }

    fn slice(&self, id: u64) -> Option<&Slice> {
        match &self.dir {
            Dir::Flat(d) => d.get(id as usize).and_then(|s| s.as_deref()),
            Dir::Super { blocks, .. } => {
                let (pos, found) = find_block(blocks, id / SUPER_BLOCK_SLOTS);
                if !found {
                    return None;
                }
                blocks[pos].slots[(id % SUPER_BLOCK_SLOTS) as usize].as_deref()
            }
        }
    }

    /// The slot for `id`, which must exist (flat: `id < dir_alloc`; super: block exists).
    fn slot_mut(&mut self, id: u64) -> Option<&mut SlicePtr> {
        match &mut self.dir {
            Dir::Flat(d) => d.get_mut(id as usize),
            Dir::Super { blocks, .. } => {
                let (pos, found) = find_block(blocks, id / SUPER_BLOCK_SLOTS);
                if !found {
                    return None;
                }
                Some(&mut blocks[pos].slots[(id % SUPER_BLOCK_SLOTS) as usize])
            }
        }
    }

    /// Take the slice out of its slot (to rebuild it), leaving the slot `None` without
    /// touching counts; [`Self::put`] puts it back.
    fn take(&mut self, id: u64) -> Option<Box<Slice>> {
        let s = self.slot_mut(id)?.take()?;
        self.alloc -= s.alloc_size();
        Some(s)
    }

    fn put(&mut self, id: u64, s: Box<Slice>) {
        self.alloc += s.alloc_size();
        *self.slot_mut(id).expect("slot taken from") = Some(s);
    }

    /// `arSetSlice` with a new slice (the slot must be empty).
    fn install(&mut self, id: u64, s: Box<Slice>) {
        self.alloc += s.alloc_size();
        match &mut self.dir {
            Dir::Flat(d) => d[id as usize] = Some(s),
            Dir::Super { blocks, cap } => {
                let block_id = id / SUPER_BLOCK_SLOTS;
                let (pos, found) = find_block(blocks, block_id);
                if !found {
                    if blocks.len() as u32 >= *cap {
                        let new_cap = if *cap == 0 { 4 } else { *cap * 2 };
                        self.alloc += usable(new_cap as usize * SDIR_ENTRY) - usable(*cap as usize * SDIR_ENTRY);
                        *cap = new_cap;
                    }
                    blocks.insert(pos, Block::new(block_id));
                    self.alloc += usable(SUPER_BLOCK_SLOTS as usize * 8);
                }
                let b = &mut blocks[pos];
                b.slots[(id % SUPER_BLOCK_SLOTS) as usize] = Some(s);
                b.count += 1;
            }
        }
        self.num_slices += 1;
    }

    /// `arSetSlice(…, NULL)` + free: drop the slice at `id`, removing an emptied block.
    fn remove_slice(&mut self, id: u64) {
        match &mut self.dir {
            Dir::Flat(d) => {
                if let Some(s) = d[id as usize].take() {
                    self.alloc -= s.alloc_size() + slice_heap_mem(&s);
                    self.num_slices -= 1;
                }
            }
            Dir::Super { blocks, .. } => {
                let (pos, found) = find_block(blocks, id / SUPER_BLOCK_SLOTS);
                if !found {
                    return;
                }
                let b = &mut blocks[pos];
                if let Some(s) = b.slots[(id % SUPER_BLOCK_SLOTS) as usize].take() {
                    self.alloc -= s.alloc_size() + slice_heap_mem(&s);
                    b.count -= 1;
                    self.num_slices -= 1;
                    if b.count == 0 {
                        blocks.remove(pos);
                        self.alloc -= usable(SUPER_BLOCK_SLOTS as usize * 8);
                    }
                }
            }
        }
    }

    /// `arDirGrow`: make room for `id` (switching to the superdir past 2048 slices).
    fn dir_grow(&mut self, id: u64) {
        if let Dir::Flat(d) = &mut self.dir
            && id >= SUPER_BLOCK_SLOTS
        {
            // arPromoteToSuperDir: the flat directory becomes block 0.
            let flat = std::mem::take(d);
            self.alloc -= usable(flat.len() * 8);
            self.alloc += usable(4 * SDIR_ENTRY);
            let mut blocks = Vec::new();
            if !flat.is_empty() {
                let mut b = Block::new(0);
                for (i, s) in flat.into_iter().enumerate() {
                    if s.is_some() {
                        b.count += 1;
                    }
                    b.slots[i] = s;
                }
                blocks.push(b);
                self.alloc += usable(SUPER_BLOCK_SLOTS as usize * 8);
            }
            self.dir = Dir::Super { blocks, cap: 4 };
        }
        if let Dir::Flat(d) = &mut self.dir
            && id as usize >= d.len()
        {
            let mut n = d.len().max(1);
            while n as u64 <= id {
                n <<= 1;
            }
            self.alloc += usable(n * 8) - usable(d.len() * 8);
            d.resize_with(n, || None);
        }
    }

    /// `arDirMaybeShrink`.
    fn dir_maybe_shrink(&mut self) {
        let Dir::Flat(d) = &mut self.dir else { return };
        if self.count == 0 || self.dir_highest_used >= d.len() as u64 * 9 / 10 {
            return;
        }
        let mut n = 1usize;
        while n as u64 <= self.dir_highest_used {
            n <<= 1;
        }
        if n >= d.len() {
            return;
        }
        self.alloc -= usable(d.len() * 8) - usable(n * 8);
        d.truncate(n);
        d.shrink_to_fit();
    }

    /// `arDirUpdateHighest` after freeing slice `freed`.
    fn update_highest(&mut self, freed: u64) {
        if self.count == 0 || freed < self.dir_highest_used {
            return;
        }
        self.dir_highest_used = match &self.dir {
            Dir::Super { .. } => self.highest_slice().unwrap_or(0),
            Dir::Flat(d) => (0..freed as usize).rev().find(|&i| d[i].is_some()).unwrap_or(0) as u64,
        };
    }

    fn highest_slice(&self) -> Option<u64> {
        match &self.dir {
            Dir::Super { blocks, .. } => blocks
                .iter()
                .rev()
                .filter(|b| b.count > 0)
                .find_map(|b| b.slots.iter().rposition(|s| s.is_some()).map(|si| b.id * SUPER_BLOCK_SLOTS + si as u64)),
            Dir::Flat(d) => d.iter().rposition(|s| s.is_some()).map(|i| i as u64),
        }
    }

    // ---- element operations ----

    pub fn get(&self, idx: u64) -> Option<&Val> {
        let (id, rel) = self.split(idx);
        self.slice(id)?.get(rel)
    }

    /// `arSet` (`idx` < `u64::MAX`).
    pub fn set(&mut self, idx: u64, v: Val, lim: Limits) {
        debug_assert!(idx != u64::MAX);
        let (id, rel) = self.split(idx);
        let slice_size = self.slice_size;
        self.dir_grow(id);
        let vmem = v.heap_mem();
        let mut s = match self.take(id) {
            Some(s) => s,
            None => {
                let s = if lim.kmax > 0 { Slice::Sparse(Sparse::new(lim)) } else { Slice::Dense(Dense::new(rel, slice_size)) };
                self.install(id, Box::new(s));
                self.take(id).expect("just installed")
            }
        };
        let mut added = false;
        let mut old_mem = 0;
        match &mut *s {
            Slice::Dense(d) => {
                d.grow_if_needed(rel, slice_size);
                let slot = &mut d.items[(rel - d.offset) as usize];
                match slot.replace(v) {
                    Some(old) => old_mem = old.heap_mem(),
                    None => {
                        d.count += 1;
                        added = true;
                    }
                }
                if rel > d.max_idx {
                    d.max_idx = rel;
                }
            }
            Slice::Sparse(sp) => {
                let (pos, found) = sp.find(rel as u16);
                if found {
                    old_mem = std::mem::replace(&mut sp.values[pos], v).heap_mem();
                } else if sp.count() >= lim.kmax {
                    let Slice::Sparse(sp) = std::mem::replace(&mut *s, Slice::Sparse(Sparse::new(lim))) else { unreachable!() };
                    let mut d = sp.promote(slice_size);
                    d.grow_if_needed(rel, slice_size);
                    d.items[(rel - d.offset) as usize] = Some(v);
                    d.count += 1;
                    if rel > d.max_idx {
                        d.max_idx = rel;
                    }
                    *s = Slice::Dense(d);
                    added = true;
                } else {
                    if sp.count() >= sp.cap {
                        sp.cap = (sp.cap * 2).min(lim.kmax);
                        sp.offsets.reserve_exact(sp.cap as usize - sp.offsets.len());
                        sp.values.reserve_exact(sp.cap as usize - sp.values.len());
                    }
                    sp.offsets.insert(pos, rel as u16);
                    sp.values.insert(pos, v);
                    added = true;
                }
            }
        }
        self.alloc = self.alloc + vmem - old_mem;
        self.put(id, s);
        if added {
            self.count += 1;
        }
        if id > self.dir_highest_used || self.count == 1 {
            self.dir_highest_used = id;
        }
    }

    /// `arDel`: true if something was deleted.
    pub fn del(&mut self, idx: u64, lim: Limits) -> bool {
        let (id, rel) = self.split(idx);
        let Some(mut s) = self.take(id) else { return false };
        let removed = match &mut *s {
            Slice::Dense(d) => {
                let hit = rel >= d.offset && rel < d.offset + d.winsize() && d.items[(rel - d.offset) as usize].is_some();
                if hit {
                    let old = d.items[(rel - d.offset) as usize].take().expect("checked");
                    self.alloc -= old.heap_mem();
                    d.count -= 1;
                    d.update_max_after_delete(rel);
                }
                hit
            }
            Slice::Sparse(sp) => {
                let (pos, found) = sp.find(rel as u16);
                if found {
                    sp.offsets.remove(pos);
                    self.alloc -= sp.values.remove(pos).heap_mem();
                }
                found
            }
        };
        if !removed {
            self.put(id, s);
            return false;
        }
        self.count -= 1;
        if s.count() != 0 {
            if matches!(*s, Slice::Dense(_)) {
                s.maybe_demote(lim);
            }
            self.put(id, s);
            return true;
        }
        self.put(id, s);
        self.remove_slice(id);
        self.update_highest(id);
        self.dir_maybe_shrink();
        true
    }

    /// `arDeleteSliceRange`: delete offsets `[lo, hi]` of slice `id`.
    fn delete_slice_range(&mut self, id: u64, lo: u32, hi: u32, lim: Limits) -> u64 {
        let Some(mut s) = self.take(id) else { return 0 };
        let mut deleted = 0u64;
        let mut freed_heap = 0;
        match &mut *s {
            Slice::Dense(d) => {
                let (wlo, whi) = (d.offset, d.offset + d.winsize() - 1);
                let (elo, ehi) = (lo.max(wlo), hi.min(whi));
                if elo <= ehi {
                    for off in elo..=ehi {
                        if let Some(v) = d.items[(off - d.offset) as usize].take() {
                            freed_heap += v.heap_mem();
                            d.count -= 1;
                            deleted += 1;
                        }
                    }
                    if d.count > 0 && d.max_idx >= elo {
                        d.max_idx = d.offset;
                        if let Some(i) = d.items.iter().rposition(|v| v.is_some()) {
                            d.max_idx = d.offset + i as u32;
                        }
                    }
                }
            }
            Slice::Sparse(sp) => {
                let first = sp.offsets.partition_point(|&o| (o as u32) < lo);
                let last = sp.offsets.partition_point(|&o| (o as u32) <= hi);
                if first < last {
                    sp.offsets.drain(first..last);
                    for v in sp.values.drain(first..last) {
                        freed_heap += v.heap_mem();
                    }
                    deleted = (last - first) as u64;
                }
            }
        }
        self.alloc -= freed_heap;
        self.count -= deleted;
        let empty = s.count() == 0;
        if !empty {
            s.maybe_demote(lim);
        }
        self.put(id, s);
        if empty {
            self.remove_slice(id);
        }
        deleted
    }

    /// `arDeleteRange`: delete every element in `[lo, hi]`, in time proportional to the slices
    /// and boundary elements touched.
    pub fn delete_range(&mut self, lo: u64, hi: u64, lim: Limits) -> u64 {
        if self.count == 0 || lo > hi {
            return 0;
        }
        let size = self.slice_size;
        let ((lo_s, lo_off), (hi_s, hi_off)) = (self.split(lo), self.split(hi));
        let mut deleted = 0;
        let mut touched_highest = false;
        if lo_s == hi_s {
            deleted = self.delete_slice_range(lo_s, lo_off, hi_off, lim);
            touched_highest |= lo_s >= self.dir_highest_used;
        } else {
            deleted += self.delete_slice_range(lo_s, lo_off, size - 1, lim);
            deleted += self.delete_slice_range(hi_s, 0, hi_off, lim);
            touched_highest |= hi_s >= self.dir_highest_used;
            if lo_s + 1 < hi_s {
                let (mid_lo, mid_hi) = (lo_s + 1, hi_s - 1);
                let ids: Vec<u64> = match &self.dir {
                    Dir::Super { blocks, .. } => {
                        let (start, _) = find_block(blocks, mid_lo / SUPER_BLOCK_SLOTS);
                        let (end, found) = find_block(blocks, mid_hi / SUPER_BLOCK_SLOTS);
                        let end = end + found as usize;
                        blocks[start..end]
                            .iter()
                            .rev()
                            .flat_map(|b| {
                                let base = b.id * SUPER_BLOCK_SLOTS;
                                let first = mid_lo.saturating_sub(base).min(SUPER_BLOCK_SLOTS - 1);
                                let last = if mid_hi < base + SUPER_BLOCK_SLOTS - 1 { mid_hi - base } else { SUPER_BLOCK_SLOTS - 1 };
                                (first..=last).filter(move |&si| b.slots[si as usize].is_some()).map(move |si| base + si)
                            })
                            .collect()
                    }
                    Dir::Flat(d) => {
                        let end = mid_hi.min(d.len() as u64 - 1);
                        (mid_lo..=end).filter(|&i| d[i as usize].is_some()).collect()
                    }
                };
                for id in ids {
                    let n = self.slice(id).map_or(0, |s| s.count()) as u64;
                    deleted += n;
                    self.count -= n;
                    self.remove_slice(id);
                    touched_highest |= id >= self.dir_highest_used;
                }
            }
        }
        if touched_highest && self.count > 0 {
            self.dir_highest_used = self.highest_slice().unwrap_or(0);
        }
        if self.count == 0 {
            self.dir_highest_used = 0;
        }
        self.dir_maybe_shrink();
        deleted
    }

    /// `arTruncate`: delete every index >= `limit`.
    pub fn truncate(&mut self, limit: u64, lim: Limits) {
        let len = self.len();
        if self.count > 0 && limit < len {
            self.delete_range(limit, len - 1, lim);
        }
    }

    /// `arMayPromoteToDenseForRangeSet`: before writing every index of `[lo, hi]`, make the
    /// slices that would overflow `kmax` dense up front.
    pub fn promote_for_range_set(&mut self, lo: u64, hi: u64, lim: Limits) {
        if lim.kmax == 0 {
            return;
        }
        let (slo, shi) = (lo >> self.bits, hi >> self.bits);
        self.dir_grow(shi);
        for id in slo..=shi {
            let start = if id == slo { lo } else { id << self.bits };
            let end = if id == shi { hi } else { ((id + 1) << self.bits) - 1 };
            let (s_off, e_off) = (self.split(start).1, self.split(end).1);
            let range = e_off - s_off + 1;
            let Some(mut s) = self.take(id) else {
                if range > lim.kmax {
                    self.install(id, Box::new(Slice::Dense(Dense::new(s_off, self.slice_size))));
                }
                continue;
            };
            if let Slice::Sparse(sp) = &*s {
                let existing = sp.offsets.iter().filter(|&&o| (o as u32) >= s_off && (o as u32) <= e_off).count() as u32;
                if range > lim.kmax || sp.count() + (range - existing) > lim.kmax {
                    let Slice::Sparse(sp) = std::mem::replace(&mut *s, Slice::Sparse(Sparse::new(lim))) else { unreachable!() };
                    *s = Slice::Dense(sp.promote(self.slice_size));
                }
            }
            self.put(id, s);
        }
    }

    /// Populated elements of `[start, end]` (descending when `start > end`).
    pub fn scan(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, &Val)> + '_ {
        let rev = start > end;
        let (lo, hi) = if rev { (end, start) } else { (start, end) };
        let (lo_s, hi_s) = (lo >> self.bits, hi >> self.bits);
        let ids: Box<dyn Iterator<Item = (u64, &Slice)> + '_> = if self.count == 0 || lo_s > self.dir_highest_used {
            Box::new(std::iter::empty())
        } else {
            let hi_s = hi_s.min(self.dir_highest_used);
            match &self.dir {
                Dir::Flat(d) => {
                    let it = (lo_s..=hi_s).filter_map(move |i| d.get(i as usize).and_then(|s| s.as_deref()).map(|s| (i, s)));
                    if rev { Box::new(it.rev()) } else { Box::new(it) }
                }
                Dir::Super { blocks, .. } => {
                    let (start, _) = find_block(blocks, lo_s / SUPER_BLOCK_SLOTS);
                    let (end, found) = find_block(blocks, hi_s / SUPER_BLOCK_SLOTS);
                    let bs = &blocks[start..end + found as usize];
                    let per = move |b: &'_ Block| {
                        let base = b.id * SUPER_BLOCK_SLOTS;
                        let first = lo_s.saturating_sub(base);
                        let last = (hi_s - base).min(SUPER_BLOCK_SLOTS - 1);
                        (first, last, base)
                    };
                    if rev {
                        Box::new(bs.iter().rev().flat_map(move |b| {
                            let (f, l, base) = per(b);
                            (f..=l).rev().filter_map(move |si| b.slots[si as usize].as_deref().map(|s| (base + si, s)))
                        }))
                    } else {
                        Box::new(bs.iter().flat_map(move |b| {
                            let (f, l, base) = per(b);
                            (f..=l).filter_map(move |si| b.slots[si as usize].as_deref().map(|s| (base + si, s)))
                        }))
                    }
                }
            }
        };
        let size = self.slice_size;
        let bits = self.bits;
        ids.flat_map(move |(id, s)| {
            let off_lo = if id == lo_s { (lo & (size as u64 - 1)) as u32 } else { 0 };
            let off_hi = if id == hi_s { (hi & (size as u64 - 1)) as u32 } else { size - 1 };
            s.items(off_lo, off_hi, rev).map(move |(off, v)| ((id << bits) | off as u64, v))
        })
    }

    /// Every element in index order (RDB, AOF rewrite).
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Val)> + '_ {
        self.scan(0, u64::MAX - 1)
    }

    /// `ARINFO` (the per-encoding figures only when `full`).
    pub fn info(&self, full: bool) -> Info {
        let mut info = Info {
            count: self.count,
            len: self.len(),
            insert_idx: self.insert_idx,
            slices: self.num_slices,
            directory_size: match &self.dir {
                Dir::Flat(d) => d.len() as u64,
                Dir::Super { cap, .. } => *cap as u64,
            },
            super_dir_entries: match &self.dir {
                Dir::Flat(_) => 0,
                Dir::Super { blocks, .. } => blocks.len() as u64,
            },
            slice_size: self.slice_size,
            dense_slices: 0,
            sparse_slices: 0,
            dense_total_winsize: 0,
            dense_total_count: 0,
            sparse_total_cap: 0,
        };
        if full {
            let mut each = |s: &Slice| match s {
                Slice::Dense(d) => {
                    info.dense_slices += 1;
                    info.dense_total_winsize += d.winsize() as u64;
                    info.dense_total_count += d.count as u64;
                }
                Slice::Sparse(sp) => {
                    info.sparse_slices += 1;
                    info.sparse_total_cap += sp.cap as u64;
                }
            };
            match &self.dir {
                Dir::Flat(d) => d.iter().flatten().for_each(|s| each(s)),
                Dir::Super { blocks, .. } => blocks.iter().flat_map(|b| b.slots.iter().flatten()).for_each(|s| each(s)),
            }
        }
        info
    }
}

fn slice_heap_mem(s: &Slice) -> usize {
    match s {
        Slice::Dense(d) => d.items.iter().flatten().map(Val::heap_mem).sum(),
        Slice::Sparse(sp) => sp.values.iter().map(Val::heap_mem).sum(),
    }
}

/// `arSuperDirFind`: lower bound on block id.
fn find_block(blocks: &[Block], id: u64) -> (usize, bool) {
    let pos = blocks.partition_point(|b| b.id < id);
    (pos, blocks.get(pos).is_some_and(|b| b.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> Limits {
        Limits::default()
    }

    fn text(v: Option<&Val>) -> Option<String> {
        v.map(|v| String::from_utf8(v.text().into_owned()).unwrap())
    }

    #[test]
    fn encoding() {
        let kind = |s: &str| match Val::encode(s.as_bytes()).decoded() {
            Decoded::Int(_) => "int",
            Decoded::Float(_) => "float",
            Decoded::Small(_) => "small",
            Decoded::Heap(_) => "heap",
        };
        assert_eq!(kind("123"), "int");
        assert_eq!(kind("-0"), "small");
        assert_eq!(kind("2305843009213693951"), "int");
        assert_eq!(kind("2305843009213693952"), "heap");
        assert_eq!(kind("1.5"), "float");
        assert_eq!(kind("1.0"), "float");
        assert_eq!(kind("-0.0"), "float");
        assert_eq!(kind("0.1"), "small");
        assert_eq!(kind("1.00"), "small");
        assert_eq!(kind("hello"), "small");
        assert_eq!(kind("hello world"), "heap");
        assert_eq!(kind(""), "small");
        for s in ["123", "-0", "1.5", "1.0", "-0.0", "0.1", "hello", "hello world", "", "1e5", "0.25", "12345678.5"] {
            assert_eq!(Val::encode(s.as_bytes()).text().as_ref(), s.as_bytes(), "{s}");
        }
        let v = Val::encode(b"a long heap string");
        let c = v.clone();
        drop(v);
        assert_eq!(c.text().as_ref(), b"a long heap string");
    }

    #[test]
    fn set_get_del_and_info() {
        let mut a = Array::new(4096);
        a.set(0, Val::encode(b"x"), lim());
        a.set(1, Val::encode(b"1.5"), lim());
        a.set(3, Val::encode(b"hello12345"), lim());
        assert_eq!((a.count(), a.len()), (3, 4));
        let i = a.info(true);
        assert_eq!((i.slices, i.directory_size, i.sparse_slices, i.sparse_total_cap), (1, 1, 1, 4));
        assert_eq!(text(a.get(3)).as_deref(), Some("hello12345"));
        assert!(a.get(2).is_none());
        for i in 4..20 {
            a.set(i, Val::encode(b"v"), lim());
        }
        let i = a.info(true);
        assert_eq!((i.dense_slices, i.dense_total_winsize), (1, 32));
        assert!(a.del(3, lim()));
        assert!(!a.del(3, lim()));
        assert_eq!(a.delete_range(0, 100, lim()), 18);
        assert_eq!((a.count(), a.len()), (0, 0));
        // The header and a one-slot directory remain.
        assert_eq!(a.mem(), 88);
    }

    #[test]
    fn superdir_and_scan() {
        let mut a = Array::new(256);
        a.set(5, Val::encode(b"a"), lim());
        a.set(10_000_000, Val::encode(b"b"), lim());
        a.set(u64::MAX - 1, Val::encode(b"c"), lim());
        let i = a.info(false);
        assert_eq!((i.slices, i.directory_size, i.super_dir_entries), (3, 4, 3));
        assert_eq!(a.len(), u64::MAX);
        let fwd: Vec<u64> = a.scan(0, u64::MAX - 1).map(|(i, _)| i).collect();
        assert_eq!(fwd, vec![5, 10_000_000, u64::MAX - 1]);
        let rev: Vec<u64> = a.scan(u64::MAX - 1, 6).map(|(i, _)| i).collect();
        assert_eq!(rev, vec![u64::MAX - 1, 10_000_000]);
        assert_eq!(a.delete_range(6, u64::MAX - 1, lim()), 2);
        assert_eq!((a.len(), a.info(false).super_dir_entries), (6, 1));
    }
}
