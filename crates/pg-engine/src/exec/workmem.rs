//! Query memory: PostgreSQL's accounting of a node's working memory against work_mem /
//! hash_mem, so that a node spills where PostgreSQL's does (and EXPLAIN ANALYZE shows its
//! method, batches and sizes). The engine's own rows are bigger than PostgreSQL's tuples;
//! what decides is PostgreSQL's count of them (`minimal_tuple_len`), which keeps a node's
//! memory within a constant factor of work_mem and its spills where the reference's are.

use crate::types::{DataType, Value};

pub const BLCKSZ: i64 = 8192;
/// A logical tape block's payload (BLCKSZ less its TapeBlockTrailer).
pub const TAPE_BLOCK_DATA: i64 = BLCKSZ - 16;
/// sizeof(SortTuple).
const SORT_TUPLE: i64 = 24;
/// ALLOC_CHUNKHDRSZ (MemoryChunk) and ALLOCSET_SEPARATE_THRESHOLD.
const CHUNK_HDR: i64 = 8;
const SEPARATE_THRESHOLD: i64 = 8192;
/// offsetof(MinimalTupleData, t_infomask2): what a tape write leaves out of a MinimalTuple.
pub const MINIMAL_TUPLE_DATA_OFFSET: i64 = 10;
/// tuplesort's INITIAL_MEMTUPSIZE, TAPE_BUFFER_OVERHEAD, MERGE_BUFFER_SIZE.
const INITIAL_MEMTUPSIZE: usize = 1024;
const TAPE_BUFFER_OVERHEAD: i64 = BLCKSZ;
const MERGE_BUFFER_SIZE: i64 = BLCKSZ * 32;

/// work_mem in bytes (at least 64 kB, as tuplesort takes it).
pub fn work_mem(ctx: &super::ExecCtx) -> i64 {
    let s = ctx.sess.settings.borrow();
    let kb = s.get_raw("work_mem").and_then(|v| v.parse::<i64>().ok()).unwrap_or(4096);
    kb.max(64) * 1024
}

/// GetMemoryChunkSpace of an aset.c chunk of `size` bytes: a power of two (at least 8)
/// with its header, or the size itself past the chunk limit.
pub fn aset_chunk(size: i64) -> i64 {
    if size > SEPARATE_THRESHOLD {
        maxalign(size) + CHUNK_HDR
    } else {
        (size.max(8) as u64).next_power_of_two() as i64 + CHUNK_HDR
    }
}

pub fn maxalign(n: i64) -> i64 {
    (n + 7) & !7
}

/// The t_len of a MinimalTuple of `row` (of columns `types`): heap_form_minimal_tuple's
/// header and null bitmap, then the values as a heap tuple lays them out.
pub fn minimal_tuple_len(row: &[Value], types: &[DataType], ctx: &super::ExecCtx) -> i64 {
    minimal_tuple_len_of(row, types, None, ctx)
}

/// `minimal_tuple_len` with the columns `mask` leaves out NULL (as the planner makes the
/// columns nothing above reads: remove_unused_subquery_outputs).
pub fn minimal_tuple_len_of(row: &[Value], types: &[DataType], mask: Option<&[bool]>, ctx: &super::ExecCtx) -> i64 {
    let kept = |i: usize| mask.is_none_or(|m| m.get(i).copied().unwrap_or(true));
    let has_null = row.iter().enumerate().any(|(i, v)| v.is_null() || !kept(i));
    // (SizeofMinimalTupleHeader 15, the bitmap, MAXALIGNed at MINIMAL_TUPLE_OFFSET 8.)
    let bitmap = if has_null { types.len().div_ceil(8) as i64 } else { 0 };
    let hoff = maxalign(15 + bitmap + 8) - 8;
    hoff + data_len(row.iter().zip(types).enumerate().filter(|(i, _)| kept(*i)).map(|(_, p)| p), ctx)
}

/// The t_len of a MinimalTuple of the columns `cols` marks only (None: all): a tuple of a
/// node's target list, the other columns not in it at all (a Hash's input, CP_SMALL_TLIST).
pub fn minimal_tuple_len_cols(row: &[Value], types: &[DataType], cols: Option<&[bool]>, ctx: &super::ExecCtx) -> i64 {
    let kept = |i: usize| cols.is_none_or(|m| m.get(i).copied().unwrap_or(false));
    let mut natts = 0usize;
    let mut has_null = false;
    for (i, v) in row.iter().enumerate() {
        if kept(i) {
            natts += 1;
            has_null |= v.is_null();
        }
    }
    let bitmap = if has_null { natts.div_ceil(8) as i64 } else { 0 };
    let hoff = maxalign(15 + bitmap + 8) - 8;
    hoff + data_len(row.iter().zip(types).enumerate().filter(|(i, _)| kept(*i)).map(|(_, p)| p), ctx)
}

/// The t_len of a MinimalTuple of the row's columns `order` lists, in that order (a target
/// list's columns aren't in the table's order: alignment padding follows theirs).
pub fn minimal_tuple_len_order(row: &[Value], types: &[DataType], order: &[usize], ctx: &super::ExecCtx) -> i64 {
    let has_null = order.iter().any(|&i| row[i].is_null());
    let bitmap = if has_null { order.len().div_ceil(8) as i64 } else { 0 };
    let hoff = maxalign(15 + bitmap + 8) - 8;
    hoff + data_len(order.iter().map(|&i| (&row[i], &types[i])), ctx)
}

/// The values' bytes as a heap tuple lays them out (NULLs take none).
fn data_len<'a>(vals: impl Iterator<Item = (&'a Value, &'a DataType)>, ctx: &super::ExecCtx) -> i64 {
    let mut off: i64 = 0;
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, ctx.fmt());
    for (v, ty) in vals {
        if v.is_null() {
            continue;
        }
        off += match (v, ty) {
            (Value::Int4(_) | Value::Date(_) | Value::Oid(_) | Value::Float4(_), _) => align(off, 4) - off + 4,
            (Value::Int8(_) | Value::Float8(_) | Value::Timestamp(_) | Value::Timestamptz(_) | Value::Time(_), _) => {
                align(off, 8) - off + 8
            }
            (Value::Int2(_), _) => align(off, 2) - off + 2,
            (Value::Bool(_) | Value::Char(_), _) => 1,
            (Value::Text(s), DataType::Text | DataType::Varchar(_)) => varlena(off, s.len() as i64),
            (Value::Bytea(b), _) => varlena(off, b.len() as i64),
            _ => crate::catalog::relsize::datum_bytes(off as usize, v, ty, &text) as i64,
        };
    }
    off
}

fn align(off: i64, a: i64) -> i64 {
    (off + a - 1) / a * a
}

/// A varlena of `payload` bytes at `off`: a short header when it fits, else aligned with
/// a 4-byte one.
fn varlena(off: i64, payload: i64) -> i64 {
    if payload < 127 { payload + 1 } else { align(off, 4) - off + payload + 4 }
}

/// tuplesort_merge_order.
pub fn merge_order(allowed: i64) -> usize {
    (allowed / (2 * TAPE_BUFFER_OVERHEAD + MERGE_BUFFER_SIZE)).clamp(6, 500) as usize
}

/// tuplesort's memory accounting (USEMEM / LACKMEM, grow_memtuples) as tuples arrive:
/// when the tuples so far must go to tape as a run.
#[derive(Clone, Debug)]
pub struct SortMem {
    pub allowed: i64,
    avail: i64,
    memtupsize: usize,
    count: usize,
    grow: bool,
    /// The tuples' bytes counted since the last run.
    tuple_mem: i64,
    /// Switched to runs on tape (inittapes): the merge order then.
    pub tapes: Option<usize>,
}

impl SortMem {
    pub fn new(allowed: i64) -> SortMem {
        SortMem {
            allowed,
            avail: allowed - array_space(INITIAL_MEMTUPSIZE),
            memtupsize: INITIAL_MEMTUPSIZE,
            count: 0,
            grow: true,
            tuple_mem: 0,
            tapes: None,
        }
    }

    /// Memory in use (allowedMem - availMem): what "Memory: NkB" shows of an in-memory sort.
    pub fn used(&self) -> i64 {
        self.allowed - self.avail
    }

    /// A tuple counted `tuplen` bytes arrives (puttuple_common): true when it and the
    /// tuples before it since the last run are dumped as a run now (dumptuples).
    pub fn put(&mut self, tuplen: i64) -> bool {
        self.avail -= tuplen;
        self.tuple_mem += tuplen;
        if self.tapes.is_none() && self.count + 1 >= self.memtupsize {
            self.grow_memtuples();
        }
        self.count += 1;
        if self.count < self.memtupsize && self.avail >= 0 {
            return false;
        }
        if self.tapes.is_none() {
            // inittapes: the tapes' buffers, when they fit.
            let n = merge_order(self.allowed);
            let space = n as i64 * TAPE_BUFFER_OVERHEAD;
            if space + array_space(self.memtupsize) < self.allowed {
                self.avail -= space;
            }
            self.tapes = Some(n);
        }
        self.avail += self.tuple_mem;
        self.tuple_mem = 0;
        self.count = 0;
        true
    }

    /// grow_memtuples.
    fn grow_memtuples(&mut self) {
        if !self.grow {
            return;
        }
        let size = self.memtupsize;
        let used = self.allowed - self.avail;
        let new = if used <= self.avail {
            size * 2
        } else {
            self.grow = false;
            let ratio = self.allowed as f64 / used as f64;
            (size as f64 * ratio) as usize
        };
        if new <= size || self.avail < (new - size) as i64 * SORT_TUPLE {
            self.grow = false;
            return;
        }
        self.avail += array_space(size) - array_space(new);
        self.memtupsize = new;
    }
}

/// A logical tape set's block numbers as logtape.c hands them out with preallocation (a
/// hash aggregation's): each tape takes block numbers in runs of 8 doubling to 128, the
/// lowest free ones first; a block read back is freed at once; the file is as big as its
/// highest block written (LogicalTapeSetBlocks, what "Disk Usage" shows).
#[derive(Debug, Default)]
pub struct TapeSet {
    allocated: i64,
    free: std::collections::BinaryHeap<std::cmp::Reverse<i64>>,
    /// nBlocksWritten.
    pub written: i64,
}

/// One tape of a `TapeSet`.
#[derive(Debug, Default)]
pub struct Tape {
    /// The preallocated block numbers, the next one last, and the list's size.
    prealloc: Vec<i64>,
    size: usize,
    /// The block being filled and its bytes.
    cur: Option<i64>,
    used: i64,
    /// Its blocks, in order, and how many have been read (and freed).
    blocks: Vec<i64>,
    read: usize,
    consumed: i64,
}

impl TapeSet {
    fn free_block(&mut self) -> i64 {
        match self.free.pop() {
            Some(std::cmp::Reverse(b)) => b,
            None => {
                self.allocated += 1;
                self.allocated - 1
            }
        }
    }

    fn next_block(&mut self, t: &mut Tape) -> i64 {
        if t.prealloc.is_empty() {
            t.size = if t.size == 0 { 8 } else { (t.size * 2).min(128) };
            let mut v: Vec<i64> = (0..t.size).map(|_| self.free_block()).collect();
            v.reverse();
            t.prealloc = v;
        }
        t.prealloc.pop().unwrap()
    }

    fn write_block(&mut self, t: &mut Tape, b: i64) {
        self.written = self.written.max(b + 1);
        t.blocks.push(b);
    }

    /// `n` bytes written to tape `t` (LogicalTapeWrite: a full block goes out when more
    /// comes).
    pub fn write(&mut self, t: &mut Tape, n: i64) {
        if t.cur.is_none() {
            t.cur = Some(self.next_block(t));
        }
        t.used += n;
        while t.used > TAPE_BLOCK_DATA {
            let next = self.next_block(t);
            let cur = t.cur.replace(next).unwrap();
            self.write_block(t, cur);
            t.used -= TAPE_BLOCK_DATA;
        }
    }

    /// LogicalTapeRewindForRead: the last block written, the unused preallocated numbers
    /// freed.
    pub fn rewind(&mut self, t: &mut Tape) {
        if let Some(c) = t.cur.take()
            && t.used > 0
        {
            self.write_block(t, c);
        }
        for b in t.prealloc.drain(..) {
            self.free.push(std::cmp::Reverse(b));
        }
        t.size = 0;
    }

    /// `n` bytes read back from tape `t`: each block freed as it is read into the buffer.
    pub fn read(&mut self, t: &mut Tape, n: i64) {
        while t.read < t.blocks.len() && t.consumed + n > t.read as i64 * TAPE_BLOCK_DATA {
            self.free.push(std::cmp::Reverse(t.blocks[t.read]));
            t.read += 1;
        }
        t.consumed += n;
    }
}

/// The memtuples array's chunk space.
fn array_space(n: usize) -> i64 {
    aset_chunk(n as i64 * SORT_TUPLE)
}

/// A tuplestore's memory as tuplestore.c counts it (USEMEM of each tuple's generation
/// chunk and of its pointer array, grow_memtuples), and where it goes to a file: what
/// EXPLAIN ANALYZE's "Storage: Memory / Disk  Maximum Storage" shows (tuplestore_get_stats).
#[derive(Clone, Debug)]
pub struct StoreMem {
    allowed: i64,
    avail: i64,
    memtupsize: usize,
    count: usize,
    grow: bool,
    /// On disk (TSS_WRITEFILE): the file's bytes.
    pub disk: Option<i64>,
    max_space: i64,
    /// It has been on disk (usedDisk: kept past tuplestore_clear).
    used_disk: bool,
    /// A trailing length word per tuple (EXEC_FLAG_BACKWARD).
    backward: bool,
}

impl StoreMem {
    pub fn new(allowed: i64, backward: bool) -> StoreMem {
        // (Its first array: 2048 pointers, over ALLOCSET_SEPARATE_THRESHOLD.)
        let memtupsize = 2048;
        StoreMem {
            allowed,
            avail: allowed - aset_chunk(memtupsize as i64 * 8),
            memtupsize,
            count: 0,
            grow: true,
            disk: None,
            max_space: 0,
            used_disk: false,
            backward,
        }
    }

    /// A tuple of t_len `tlen` is stored (tuplestore_puttuple): true when this one moved
    /// the store to its file.
    pub fn put(&mut self, tlen: i64) -> bool {
        if let Some(d) = &mut self.disk {
            *d += tlen - MINIMAL_TUPLE_DATA_OFFSET + 4 + if self.backward { 4 } else { 0 };
            return false;
        }
        // (A generation context's chunk: MAXALIGNed with its header.)
        self.avail -= maxalign(tlen) + CHUNK_HDR;
        if self.count + 1 >= self.memtupsize {
            self.grow_memtuples();
        }
        self.count += 1;
        if self.count < self.memtupsize && self.avail >= 0 {
            return false;
        }
        self.max_space = self.max_space.max(self.allowed - self.avail);
        self.disk = Some(0);
        true
    }

    /// The tuples held so far go to the file (dumptuples): their t_lens.
    pub fn dumped(&mut self, tlen: i64) {
        if let Some(d) = &mut self.disk {
            *d += tlen - MINIMAL_TUPLE_DATA_OFFSET + 4 + if self.backward { 4 } else { 0 };
        }
    }

    fn grow_memtuples(&mut self) {
        if !self.grow {
            return;
        }
        let size = self.memtupsize;
        let used = self.allowed - self.avail;
        let new = if used <= self.avail {
            size * 2
        } else {
            self.grow = false;
            (size as f64 * (self.allowed as f64 / used as f64)) as usize
        };
        if new <= size || self.avail < (new - size) as i64 * 8 {
            return;
        }
        self.avail += aset_chunk(size as i64 * 8) - aset_chunk(new as i64 * 8);
        self.memtupsize = new;
    }

    /// tuplestore_get_stats: (on disk, maximum bytes).
    pub fn stats(&self) -> (bool, i64) {
        let mut s = self.clone();
        s.update_max();
        (s.used_disk, s.max_space)
    }

    /// tuplestore_updatemax: the space in use now joins the maximum.
    fn update_max(&mut self) {
        match self.disk {
            Some(d) => {
                self.max_space = self.max_space.max(d);
                self.used_disk = true;
            }
            None => self.max_space = self.max_space.max(self.allowed - self.avail),
        }
    }

    /// tuplestore_trim's frees: the tuples of t_len `tlens` released (the maximum taken
    /// first), and `compact` of them out of the pointer array (its memmove, when it frees an
    /// eighth of the tuples or more).
    pub fn trim(&mut self, tlens: &[i64], compact: usize) {
        self.update_max();
        for &t in tlens {
            self.avail += maxalign(t) + CHUNK_HDR;
        }
        self.count -= compact;
    }

    /// The tuples in the pointer array (memtupcount).
    pub fn count(&self) -> usize {
        self.count
    }

    /// tuplestore_clear: every tuple gone (the maximum taken first), the file closed, the
    /// pointer array kept at its size.
    pub fn clear(&mut self) {
        self.update_max();
        self.disk = None;
        self.count = 0;
        self.avail = self.allowed - aset_chunk(self.memtupsize as i64 * 8);
    }
}

/// A WindowAgg's tuplestore (nodeWindowAgg.c): each partition's rows spooled as the window
/// needs them, the rows no read pointer can reach again trimmed after each row
/// (tuplestore_trim), the store cleared between partitions (release_partition). What
/// EXPLAIN ANALYZE's "Storage" shows of it.
#[derive(Clone, Debug)]
pub struct WinStore {
    pub mem: StoreMem,
}

impl WinStore {
    pub fn new(allowed: i64, backward: bool) -> WinStore {
        WinStore { mem: StoreMem::new(allowed, backward) }
    }

    /// One partition of tuples of t_len `tlens`: before the trim after row `r`, `spool(r)`
    /// rows have been read into the store, and the oldest read pointer is at `oldest(r)`
    /// (both clamped to the partition).
    pub fn partition(&mut self, tlens: &[i64], spool: impl Fn(usize) -> usize, oldest: impl Fn(usize) -> usize) {
        let n = tlens.len();
        // (Rows read in, freed, and moved out of the pointer array: absolute positions.)
        let (mut spooled, mut deleted, mut base) = (0usize, 0usize, 0usize);
        for r in 0..n {
            let want = spool(r).clamp(r + 1, n);
            while spooled < want {
                if self.mem.put(tlens[spooled]) {
                    for &t in &tlens[deleted..=spooled] {
                        self.mem.dumped(t);
                    }
                }
                spooled += 1;
            }
            // (A store on disk isn't trimmed. The tuple before the oldest pointer stays: the
            // caller may still hold it.)
            if self.mem.disk.is_some() {
                continue;
            }
            let oldest = oldest(r).min(r + 1).min(spooled);
            let nremove = oldest.saturating_sub(1);
            if nremove <= base {
                continue;
            }
            let freed = &tlens[deleted.max(base)..nremove.max(deleted)];
            let compact = if nremove - base >= (spooled - base) / 8 { nremove - base } else { 0 };
            self.mem.trim(freed, compact);
            deleted = deleted.max(nremove);
            base += compact;
        }
        self.mem.clear();
    }

    /// (on disk, maximum bytes).
    pub fn stats(&self) -> (bool, i64) {
        self.mem.stats()
    }
}

/// Logical tape blocks of tapes holding `bytes` each (their runs back to back): what
/// LogicalTapeSetBlocks counts once runs are built.
pub fn tape_blocks(bytes: &[i64]) -> i64 {
    bytes.iter().map(|b| (b + TAPE_BLOCK_DATA - 1) / TAPE_BLOCK_DATA).sum()
}

/// A memory context's blocks as aset.c / bump.c grow them (MemoryContextMemAllocated): a
/// first block of 8 kB, then blocks from 8 kB doubling up to the context's maximum, a chunk
/// past the chunk limit in a block of its own.
#[derive(Clone, Debug)]
pub struct Blocks {
    pub allocated: i64,
    free: i64,
    next: i64,
    max: i64,
    limit: i64,
    /// The first block's header (the context's and the block's), and a block's.
    first_hdr: i64,
    hdr: i64,
}

impl Blocks {
    pub fn new(max: i64, first_hdr: i64, hdr: i64, limit: i64) -> Blocks {
        Blocks { allocated: 8192, free: 8192 - first_hdr, next: 8192, max, limit, first_hdr, hdr }
    }

    /// A bump.c context (HashAgg table context): no chunk headers.
    pub fn bump(max: i64) -> Blocks {
        let mut limit = max;
        while limit > (max - 32) / 8 {
            limit >>= 1;
        }
        Blocks::new(max, 152, 32, limit)
    }

    /// An aset.c context with blocks up to `max`.
    pub fn aset(max: i64) -> Blocks {
        Blocks::new(max, 264, 40, SEPARATE_THRESHOLD)
    }

    /// A chunk of `size` bytes, rounded as the context rounds them.
    pub fn alloc(&mut self, size: i64) {
        if size > self.limit {
            self.allocated += size + self.hdr + CHUNK_HDR;
            return;
        }
        if size > self.free {
            let mut b = self.next;
            self.next = (self.next * 2).min(self.max);
            while b < size + self.hdr {
                b *= 2;
            }
            self.allocated += b;
            self.free = b - self.hdr;
        }
        self.free -= size;
    }

    /// MemoryContextReset: the first block kept.
    pub fn reset(&mut self) {
        self.allocated = 8192;
        self.free = 8192 - self.first_hdr;
        self.next = 8192;
    }
}

/// A HashAggregate's memory as nodeAgg counts it (hash_agg_check_limits): the meta context
/// (the hash table's bucket array), the table context (each group's MinimalTuple with its
/// per-group states) and the per-tuple context (by-reference transition values).
#[derive(Clone, Debug)]
pub struct HashAggMem {
    pub meta: i64,
    pub entries: Blocks,
    pub tvals: Blocks,
    /// MAXALIGN(numtrans * sizeof(AggStatePerGroupData)), and the chunks a group's
    /// by-reference transition states take (`agg_state_chunks`; none: by value).
    pub additional: i64,
    pub trans_chunks: Vec<i64>,
    pub ngroups: u64,
    pub mem_limit: i64,
    pub ngroups_limit: u64,
    /// hash_mem_peak.
    pub peak: i64,
}

impl HashAggMem {
    pub fn new(work_mem: i64, num_trans: usize, trans_chunks: Vec<i64>) -> HashAggMem {
        // (The table context's blocks: work_mem / 16 rounded down to a power of two,
        // between 8 kB and 8 MB.)
        let w = (work_mem / 16).max(1) as u64;
        let max = (1u64 << (63 - w.leading_zeros())) as i64;
        let max = max.clamp(8192, 8 << 20);
        HashAggMem {
            meta: 8192,
            entries: Blocks::bump(max),
            tvals: Blocks::aset(max),
            additional: maxalign(16 * num_trans as i64),
            trans_chunks,
            ngroups: 0,
            mem_limit: i64::MAX,
            ngroups_limit: u64::MAX,
            peak: 0,
        }
    }

    /// The hash table's bucket array holds `slots` entries (SH_CREATE / SH_GROW).
    pub fn table(&mut self, slots: u64) {
        let arr = slots as i64 * 16;
        self.meta = 8192 + if arr > SEPARATE_THRESHOLD { arr + 48 } else if arr > 4096 { 8192 } else { 0 };
    }

    pub fn total(&self) -> i64 {
        self.meta + self.entries.allocated + self.tvals.allocated
    }

    /// A new group of a `tuple_len`-byte key tuple: whether the node goes into spill mode
    /// now (hash_agg_check_limits, after the group is counted).
    pub fn new_group(&mut self, tuple_len: i64) -> bool {
        self.entries.alloc(maxalign(tuple_len + self.additional));
        for &c in &self.trans_chunks {
            self.tvals.alloc(c);
        }
        self.ngroups += 1;
        self.total() > self.mem_limit || self.ngroups > self.ngroups_limit
    }

    /// hash_agg_update_metrics with `buffers` bytes of tape buffers: the peak, and the
    /// entry size the next limits use.
    pub fn update(&mut self, buffers: i64) -> Option<f64> {
        self.peak = self.peak.max(self.total() + buffers);
        (self.ngroups > 0).then(|| 16.0 + self.tvals.allocated as f64 / self.ngroups as f64)
    }

    /// The table emptied for the next batch (its bucket array kept).
    pub fn reset(&mut self) {
        self.entries.reset();
        self.tvals.reset();
        self.ngroups = 0;
    }
}
