//! RediSearch's inverted index storage (`redisearch_rs/inverted_index`), byte for byte: blocks of
//! delta-encoded records, the conservative growth of their buffers (`ControlledCursor`) and of
//! the block array (`ThinVec`), so `memory_usage` and every `mem_growth` match what the
//! reference adds to `inverted_sz`.

pub mod codec;
pub mod varint;

#[cfg(test)]
mod tests;

pub use codec::{Encoding, Tracking};
pub use varint::{OffsetsIter, OffsetsWriter, offsets};

pub type DocId = u64;
pub type FieldMask = u128;

/// `IndexFlags` (spec.h).
pub mod flags {
    pub const STORE_TERM_OFFSETS: u32 = 0x01;
    pub const STORE_FIELD_FLAGS: u32 = 0x02;
    pub const HAS_MULTI_VALUE: u32 = 0x04;
    pub const HAS_CUSTOM_STOPWORDS: u32 = 0x08;
    pub const STORE_FREQS: u32 = 0x10;
    pub const STORE_NUMERIC: u32 = 0x20;
    pub const STORE_BYTE_OFFSETS: u32 = 0x40;
    pub const WIDE_SCHEMA: u32 = 0x80;
    pub const DOC_IDS_ONLY: u32 = 0x00;
    /// `INDEX_STORAGE_MASK`: the flags that pick the encoder.
    pub const STORAGE_MASK: u32 =
        STORE_FREQS | STORE_FIELD_FLAGS | STORE_TERM_OFFSETS | STORE_NUMERIC | WIDE_SCHEMA | DOC_IDS_ONLY;
}

/// `RSIndexResult` as the encoders see it; decoding yields the same shape with `offsets`
/// borrowed from the block.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Record<'a> {
    pub doc_id: DocId,
    pub freq: u32,
    pub field_mask: FieldMask,
    /// A term's VVW-encoded positions (`OffsetsWriter::bytes`).
    pub offsets: &'a [u8],
    pub numeric: f64,
}

impl<'a> Record<'a> {
    /// `build_virt`: a bare doc id (doc-ids-only, tag, missing and existing docs indexes).
    pub const fn doc(doc_id: DocId) -> Self {
        Self { doc_id, freq: 0, field_mask: 0, offsets: &[], numeric: 0.0 }
    }

    /// A term occurrence, as `InvertedIndex_WriteForwardIndexEntry` builds it.
    pub const fn term(doc_id: DocId, freq: u32, field_mask: FieldMask, offsets: &'a [u8]) -> Self {
        Self { doc_id, freq, field_mask, offsets, numeric: 0.0 }
    }

    /// `build_numeric`, as `InvertedIndex_WriteNumericEntry` builds it.
    pub const fn numeric(doc_id: DocId, value: f64) -> Self {
        Self { doc_id, freq: 1, field_mask: FieldMask::MAX, offsets: &[], numeric: value }
    }
}

/// `AddRecordOutcome`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddOutcome {
    /// Bytes the index's memory usage grew by.
    pub mem_growth: u32,
    /// New blocks the write created (0 to 2).
    pub blocks_added: u32,
}

/// `Summary` (`IISummary`): `InvertedIndex_Summary`'s header for FT.DEBUG.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub number_of_docs: u32,
    pub number_of_entries: usize,
    pub last_doc_id: DocId,
    pub flags: u32,
    pub number_of_blocks: usize,
    pub block_efficiency: f64,
    pub has_efficiency: bool,
}

/// `GcApplyInfo` (`II_GCScanStats`): what a GC pass over an index changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcInfo {
    pub bytes_freed: usize,
    pub bytes_allocated: usize,
    /// Records removed, duplicates included.
    pub entries_removed: usize,
    pub block_count_delta: i64,
}

/// `RepairType`: a block's replacement (none: deleted) and the docs it lost.
struct Repair {
    blocks: Vec<IndexBlock>,
    docs_removed: u32,
}

/// `IndexBlock`: records for a doc id range, with the capacity its buffer would have in the
/// reference (tracked here so it never depends on the allocator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexBlock {
    first_doc_id: DocId,
    last_doc_id: DocId,
    num_entries: u16,
    buf: Vec<u8>,
    cap: usize,
}

/// `size_of::<IndexBlock>()`: two doc ids, the buffer `Vec` and the u16 count, padded.
pub const BLOCK_SIZE: usize = 48;
/// The `ThinVec<IndexBlock, u32>` allocation's header (len and capacity as u32).
pub const BLOCKS_HEADER: usize = 8;
/// `ControlledCursor`'s largest single growth step.
const MAX_GROWTH_STEP: usize = 1024 * 1024;

/// `reserve_and_pad`'s capacity for `desired` bytes starting from `cap`: grow by a fifth plus
/// one (at most 1 MiB) at a time.
pub const fn grown_capacity(mut cap: usize, desired: usize) -> usize {
    while cap < desired {
        let step = 1 + cap / 5;
        cap += if step < MAX_GROWTH_STEP { step } else { MAX_GROWTH_STEP };
    }
    cap
}

/// A repaired block's buffer capacity once the parent has read it back from the fork GC's
/// pipe: serde's `Vec<u8>` visitor preallocates the announced length (at most 1 MiB), then
/// doubles.
pub const fn deserialized_capacity(len: usize) -> usize {
    let mut cap = if len < MAX_GROWTH_STEP { len } else { MAX_GROWTH_STEP };
    while cap < len {
        cap *= 2;
    }
    cap
}

impl IndexBlock {
    const fn new(doc_id: DocId) -> Self {
        Self { first_doc_id: doc_id, last_doc_id: doc_id, num_entries: 0, buf: Vec::new(), cap: 0 }
    }

    pub const fn first_doc_id(&self) -> DocId {
        self.first_doc_id
    }

    pub const fn last_doc_id(&self) -> DocId {
        self.last_doc_id
    }

    pub const fn num_entries(&self) -> u16 {
        self.num_entries
    }

    /// The encoded records.
    pub fn data(&self) -> &[u8] {
        &self.buf
    }

    /// The buffer's capacity in the reference.
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    /// `IndexBlock::mem_usage`.
    pub const fn mem_usage(&self) -> usize {
        BLOCK_SIZE + self.cap
    }

    /// Writes through `ControlledCursor`, returning how much the capacity grew.
    fn append(&mut self, bytes: &[u8]) -> usize {
        let old = self.cap;
        let desired = self.buf.len() + bytes.len();
        if desired > self.cap {
            self.cap = grown_capacity(self.cap, desired);
            self.buf.reserve_exact(self.cap - self.buf.len());
        }
        self.buf.extend_from_slice(bytes);
        self.cap - old
    }
}

/// The opaque `InvertedIndex`: its encoder, flags, blocks and the wrapper's field mask or entry
/// count.
#[derive(Clone, Debug, PartialEq)]
pub struct InvertedIndex {
    encoding: Encoding,
    flags: u32,
    blocks: Vec<IndexBlock>,
    /// The `ThinVec`'s capacity: grown one block at a time, never shrunk here.
    blocks_cap: usize,
    n_unique_docs: u32,
    field_mask: FieldMask,
    n_entries: usize,
}

/// `NewInvertedIndex_Ex`: the index and its initial memory size (`*mem_size`).
pub fn new_inverted_index(flags: u32, raw_doc_id_encoding: bool, compress_floats: bool) -> Option<(InvertedIndex, usize)> {
    let ii = InvertedIndex::new(flags, raw_doc_id_encoding, compress_floats)?;
    let size = ii.memory_usage();
    Some((ii, size))
}

impl InvertedIndex {
    /// `NewInvertedIndex_Ex`; `None` for flags without a storage layout.
    pub fn new(flags: u32, raw_doc_id_encoding: bool, compress_floats: bool) -> Option<Self> {
        Encoding::from_flags(flags, raw_doc_id_encoding, compress_floats).map(|e| Self::with_encoding(e, flags))
    }

    /// An index using `encoding` directly (e.g. the numeric range tree's `EntriesTrackingIndex`).
    pub const fn with_encoding(encoding: Encoding, flags: u32) -> Self {
        Self {
            encoding,
            flags,
            blocks: Vec::new(),
            blocks_cap: 0,
            n_unique_docs: 0,
            field_mask: 0,
            n_entries: 0,
        }
    }

    pub const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The creation flags, plus `HAS_MULTI_VALUE` once a doc id repeats.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    pub fn blocks(&self) -> &[IndexBlock] {
        &self.blocks
    }

    /// `InvertedIndex_NumBlocks`.
    pub fn number_of_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// `InvertedIndex_NumDocs`.
    pub const fn unique_docs(&self) -> u32 {
        self.n_unique_docs
    }

    /// `InvertedIndex_NumEntries`: records added to a numeric index (duplicates included), 0 for
    /// the others.
    pub const fn number_of_entries(&self) -> usize {
        self.n_entries
    }

    /// `InvertedIndex_FieldMask`: every field seen, 0 without `STORE_FIELD_FLAGS`.
    pub const fn field_mask(&self) -> FieldMask {
        self.field_mask
    }

    pub fn last_doc_id(&self) -> Option<DocId> {
        self.blocks.last().map(|b| b.last_doc_id)
    }

    /// `InvertedIndex_MemUsage`: the block array allocation, every block buffer's capacity and
    /// the struct itself.
    pub fn memory_usage(&self) -> usize {
        let heap = if self.blocks_cap == 0 { 0 } else { BLOCKS_HEADER + BLOCK_SIZE * self.blocks_cap };
        heap + self.blocks.iter().map(|b| b.cap).sum::<usize>() + self.encoding.base_size()
    }

    /// `InvertedIndex::add_record` through its wrapper: a repeated doc id is dropped unless the
    /// encoder allows duplicates (then it joins the last block and marks the index multi-value);
    /// a full block or a delta too wide for the encoder starts a new block.
    pub fn add_record(&mut self, rec: &Record<'_>) -> AddOutcome {
        let same_doc = self.last_doc_id() == Some(rec.doc_id);
        let out = if same_doc && !self.encoding.allow_duplicates() { AddOutcome::default() } else { self.write(rec, same_doc) };
        match self.encoding.tracking() {
            Tracking::FieldMask => self.field_mask |= rec.field_mask,
            Tracking::Entries => self.n_entries += 1,
            Tracking::None => {}
        }
        out
    }

    fn write(&mut self, rec: &Record<'_>, same_doc: bool) -> AddOutcome {
        let doc_id = rec.doc_id;
        let blocks_before = self.blocks.len();
        let full = !same_doc && self.blocks.last().is_some_and(|b| b.num_entries >= self.encoding.recommended_block_entries());
        let popped = if full { None } else { self.blocks.pop() };
        let mut block = popped.unwrap_or_else(|| IndexBlock::new(doc_id));
        let mut mem = 0;
        let base = if self.encoding.delta_from_first() { block.first_doc_id } else { block.last_doc_id };
        let mut delta = doc_id.wrapping_sub(base);
        if !self.encoding.delta_fits(delta) {
            mem += self.push_block(std::mem::replace(&mut block, IndexBlock::new(doc_id)));
            delta = 0;
        }
        let mut bytes = Vec::with_capacity(24 + rec.offsets.len());
        self.encoding.encode(&mut bytes, delta, rec);
        mem += block.append(&bytes);
        block.num_entries = block.num_entries.wrapping_add(1);
        block.last_doc_id = doc_id;
        mem += self.push_block(block);
        if same_doc {
            self.flags |= flags::HAS_MULTI_VALUE;
        } else {
            self.n_unique_docs += 1;
        }
        AddOutcome { mem_growth: mem as u32, blocks_added: (self.blocks.len() - blocks_before) as u32 }
    }

    /// `add_block`: the block array grows by exactly one slot when full; its first allocation
    /// also counts the `ThinVec` header.
    fn push_block(&mut self, block: IndexBlock) -> usize {
        let growth = if self.blocks.len() == self.blocks_cap {
            self.blocks_cap += 1;
            if self.blocks_cap == 1 { BLOCKS_HEADER + BLOCK_SIZE } else { BLOCK_SIZE }
        } else {
            0
        };
        self.blocks.push(block);
        growth
    }

    /// `IndexReader`: every record in doc id order.
    pub fn reader(&self) -> Reader<'_> {
        Reader::new(self)
    }

    /// `InvertedIndex::summary` (FT.DEBUG's header): an entries-tracking index reports its
    /// entries and blocks efficiency, the others their docs as entries.
    pub fn summary(&self) -> Summary {
        let tracked = self.encoding.tracking() == Tracking::Entries;
        let number_of_blocks = self.blocks.len();
        let number_of_entries = if tracked { self.n_entries } else { self.n_unique_docs as usize };
        Summary {
            number_of_docs: self.n_unique_docs,
            number_of_entries,
            last_doc_id: self.last_doc_id().unwrap_or(0),
            flags: self.flags,
            number_of_blocks,
            block_efficiency: if tracked && number_of_blocks > 0 { number_of_entries as f64 / number_of_blocks as f64 } else { 0.0 },
            has_efficiency: tracked,
        }
    }

    /// The records of one block.
    fn block_records(&self, i: usize) -> Vec<Record<'_>> {
        let blk = &self.blocks[i];
        let mut out = Vec::with_capacity(blk.num_entries as usize);
        let (mut pos, mut last) = (0, blk.first_doc_id);
        while pos < blk.buf.len() {
            let base = if self.encoding.delta_from_first() { blk.first_doc_id } else { last };
            let Some(rec) = self.encoding.decode(&blk.buf, &mut pos, base) else { break };
            last = rec.doc_id;
            out.push(rec);
        }
        out
    }

    /// `IndexBlock::repair`: the block's surviving records re-encoded into fresh blocks, `None`
    /// when nothing in it is gone. `kept` sees every surviving record.
    fn repair(&self, i: usize, exists: &dyn Fn(DocId) -> bool, kept: &mut dyn FnMut(&Record<'_>)) -> Option<Repair> {
        let mut tmp = InvertedIndex::with_encoding(self.encoding, flags::DOC_IDS_ONLY);
        let (mut unique_read, mut unique_write, mut changed) = (0u32, 0u32, false);
        let mut last: Option<DocId> = None;
        for rec in self.block_records(i) {
            let new_doc = last != Some(rec.doc_id);
            if exists(rec.doc_id) {
                kept(&rec);
                tmp.add_record(&rec);
                if new_doc {
                    unique_write += 1;
                }
            } else {
                changed = true;
            }
            if new_doc {
                unique_read += 1;
            }
            last = Some(rec.doc_id);
        }
        if tmp.blocks.is_empty() {
            Some(Repair { blocks: Vec::new(), docs_removed: unique_read })
        } else if changed {
            Some(Repair { blocks: tmp.blocks, docs_removed: unique_read - unique_write })
        } else {
            None
        }
    }

    /// The fork GC's `scan_gc` and the parent's `apply_gc` back to back (nothing can change in
    /// between here): blocks holding records of documents gone are rebuilt from their surviving
    /// records, or dropped; the block array shrinks to fit. `None` when nothing was collected.
    /// `kept` sees every surviving record of the repaired blocks.
    pub fn gc(&mut self, exists: &dyn Fn(DocId) -> bool, kept: &mut dyn FnMut(&Record<'_>)) -> Option<GcInfo> {
        let repairs: Vec<(usize, Repair)> = (0..self.blocks.len()).filter_map(|i| self.repair(i, exists, kept).map(|r| (i, r))).collect();
        if repairs.is_empty() {
            return None;
        }
        let mut info = GcInfo::default();
        let before = self.blocks.len();
        let old = std::mem::take(&mut self.blocks);
        let mut repairs = repairs.into_iter().peekable();
        for (i, block) in old.into_iter().enumerate() {
            match repairs.next_if(|(at, _)| *at == i) {
                Some((_, r)) => {
                    info.entries_removed += block.num_entries as usize;
                    info.bytes_freed += block.mem_usage();
                    self.n_unique_docs -= r.docs_removed;
                    for mut b in r.blocks {
                        b.cap = deserialized_capacity(b.buf.len());
                        info.entries_removed -= b.num_entries as usize;
                        info.bytes_allocated += b.mem_usage();
                        self.blocks.push(b);
                    }
                }
                None => self.blocks.push(block),
            }
        }
        // `shrink_to_fit`; freeing the whole array also frees its header.
        if self.blocks.is_empty() && self.blocks_cap > 0 {
            info.bytes_freed += BLOCKS_HEADER;
        }
        self.blocks_cap = self.blocks.len();
        info.block_count_delta = self.blocks.len() as i64 - before as i64;
        if self.encoding.tracking() == Tracking::Entries {
            self.n_entries -= info.entries_removed;
        }
        Some(info)
    }

    /// Every record, decoded.
    pub fn records(&self) -> Vec<Record<'_>> {
        self.reader().collect()
    }
}

/// `IndexReaderCore`: walks the blocks decoding records, `seek` jumps by block ranges first.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    ii: &'a InvertedIndex,
    block: usize,
    pos: usize,
    last_doc_id: DocId,
}

impl<'a> Reader<'a> {
    pub fn new(ii: &'a InvertedIndex) -> Self {
        let last_doc_id = ii.blocks.first().map_or(0, |b| b.first_doc_id);
        Self { ii, block: 0, pos: 0, last_doc_id }
    }

    fn set_block(&mut self, i: usize) {
        self.block = i;
        self.pos = 0;
        self.last_doc_id = self.ii.blocks[i].first_doc_id;
    }

    /// `next_record`.
    pub fn next_record(&mut self) -> Option<Record<'a>> {
        let ii = self.ii;
        loop {
            let blk = ii.blocks.get(self.block)?;
            if self.pos >= blk.buf.len() {
                if self.block + 1 >= ii.blocks.len() {
                    return None;
                }
                self.set_block(self.block + 1);
                continue;
            }
            let base = if ii.encoding.delta_from_first() { blk.first_doc_id } else { self.last_doc_id };
            let rec = ii.encoding.decode(&blk.buf, &mut self.pos, base)?;
            self.last_doc_id = rec.doc_id;
            return Some(rec);
        }
    }

    /// `skip_to`: moves to the first block that can hold `doc_id` (restarting it); false when
    /// every record is below it.
    pub fn skip_to(&mut self, doc_id: DocId) -> bool {
        let blocks = &self.ii.blocks;
        let Some(last) = blocks.last() else { return false };
        if blocks[self.block].last_doc_id >= doc_id {
            return true;
        }
        if last.last_doc_id < doc_id {
            return false;
        }
        let start = self.block + 1;
        let rel = blocks[start..].partition_point(|b| b.last_doc_id < doc_id);
        self.set_block(start + rel);
        true
    }

    /// `seek_record`: the first record from here on with a doc id of at least `doc_id`.
    pub fn seek(&mut self, doc_id: DocId) -> Option<Record<'a>> {
        if !self.skip_to(doc_id) {
            return None;
        }
        loop {
            let rec = self.next_record()?;
            if rec.doc_id >= doc_id {
                return Some(rec);
            }
        }
    }

    /// `reset`: back to the first record.
    pub fn reset(&mut self) {
        *self = Self::new(self.ii);
    }

    /// The doc id last read (the current block's first before any).
    pub const fn last_doc_id(&self) -> DocId {
        self.last_doc_id
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Record<'a>;

    fn next(&mut self) -> Option<Record<'a>> {
        self.next_record()
    }
}
