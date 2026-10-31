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
    let mut off: i64 = 0;
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, ctx.fmt());
    for (i, (v, ty)) in row.iter().zip(types).enumerate() {
        if v.is_null() || !kept(i) {
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
    hoff + off
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

/// The memtuples array's chunk space.
fn array_space(n: usize) -> i64 {
    aset_chunk(n as i64 * SORT_TUPLE)
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
    /// MAXALIGN(numtrans * sizeof(AggStatePerGroupData)), and a group's by-reference
    /// transition space (0: by value).
    pub additional: i64,
    pub trans_space: i64,
    pub ngroups: u64,
    pub mem_limit: i64,
    pub ngroups_limit: u64,
    /// hash_mem_peak.
    pub peak: i64,
}

impl HashAggMem {
    pub fn new(work_mem: i64, num_trans: usize, trans_space: f64) -> HashAggMem {
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
            trans_space: trans_space as i64,
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
        if self.trans_space > 0 {
            self.tvals.alloc(aset_chunk(self.trans_space));
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
