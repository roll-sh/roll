//! A hash join past hash_mem: nodeHash.c's hybrid hash join (the private, serial table).
//! The inner rows are counted as nodeHash counts them (HJTUPLE_OVERHEAD and the
//! MinimalTuple of the Hash's target list, the bucket array); a table past its space
//! doubles its batches (or, first, its space: ExecHashIncreaseBatchSize), keeps the rows of
//! its batch and writes the others to their batch's file, walking its dense chunks as
//! ExecHashIncreaseNumBatches does. Outer rows of later batches go to their batch's outer
//! file; each later batch is then loaded and probed in turn (ExecHashJoinNewBatch). So the
//! rows come out in PostgreSQL's order, and EXPLAIN ANALYZE's Buckets / Batches / Memory
//! Usage are its numbers. The skew table keeps the outer column's most common values'
//! inner rows in batch 0 until its buckets exceed their share of memory.

use super::pghash::JoinTable;
use super::spill::{RowReader, TempAcct, TempFile, put_tagged_row};
use super::workmem::BLCKSZ;
use super::{ExecCtx, Row};
use crate::error::Result;
use crate::types::Value;

const HJTUPLE_OVERHEAD: i64 = 16;
const HASH_CHUNK_SIZE: i64 = 32 * 1024;
const HASH_CHUNK_THRESHOLD: i64 = HASH_CHUNK_SIZE / 4;
/// MaxAllocSize / sizeof(HashJoinTuple).
const MAX_POINTERS: u64 = ((1 << 30) - 1) / 8;
/// The engine's buffer per batch file (a BufFile's is a block).
const FILE_BUF: usize = 8192;

pub const INNER: usize = 0;
pub const OUTER: usize = 1;

/// One of PostgreSQL's batch files (a BufFile of hash values and MinimalTuples): its rows
/// as chunks of the engine's spill file, and PostgreSQL's bytes.
struct BatchFile {
    chunks: Vec<(u64, u64)>,
    buf: Vec<u8>,
    acct: Option<TempAcct>,
    cleanup: std::rc::Rc<super::spill::TempCleanup>,
    bytes: u64,
}

impl Drop for BatchFile {
    fn drop(&mut self) {
        if let Some(acct) = self.acct.take() {
            acct.close_later(self.bytes, &self.cleanup);
        }
    }
}

/// The batch files, inner and outer per batch, in one file of the engine's.
#[derive(Default)]
struct Files {
    file: Option<TempFile>,
    batches: Vec<[Option<BatchFile>; 2]>,
}

impl Files {
    fn has(&self, batch: usize, side: usize) -> bool {
        self.batches.get(batch).is_some_and(|b| b[side].is_some())
    }

    /// ExecHashJoinSaveTuple: the file made at its first tuple.
    fn save(&mut self, batch: usize, side: usize, hash: u32, row: &[Value], tlen: i64, ctx: &ExecCtx) -> Result<()> {
        if self.batches.len() <= batch {
            self.batches.resize_with(batch + 1, Default::default);
        }
        let f = self.batches[batch][side].get_or_insert_with(|| BatchFile {
            chunks: Vec::new(),
            buf: Vec::new(),
            acct: Some(TempAcct::new(ctx)),
            cleanup: ctx.temp_cleanup.clone(),
            bytes: 0,
        });
        put_tagged_row(&mut f.buf, [hash, tlen as u32], row);
        f.bytes += 4 + tlen as u64;
        // (BufFile writes its buffer out a block at a time.)
        f.acct.as_ref().unwrap().pg_size(f.bytes / BLCKSZ as u64 * BLCKSZ as u64)?;
        if f.buf.len() >= FILE_BUF {
            let file = match &mut self.file {
                Some(file) => file,
                None => self.file.insert(TempFile::create_raw(ctx)?),
            };
            push_chunk(&mut f.chunks, file.append(&f.buf)?);
            f.buf.clear();
        }
        Ok(())
    }

    /// A batch's file of `side`, rewound to be read (BufFileSeek flushes its buffer).
    fn take(&mut self, batch: usize, side: usize, ctx: &ExecCtx) -> Result<Option<BatchReader>> {
        let Some(mut f) = self.batches.get_mut(batch).and_then(|b| b[side].take()) else {
            return Ok(None);
        };
        f.acct.as_ref().unwrap().pg_size(f.bytes)?;
        f.acct.as_ref().unwrap().pg_read(f.bytes);
        if !f.buf.is_empty() {
            let file = match &mut self.file {
                Some(file) => file,
                None => self.file.insert(TempFile::create_raw(ctx)?),
            };
            push_chunk(&mut f.chunks, file.append(&f.buf)?);
            f.buf = Vec::new();
        }
        f.chunks.reverse();
        Ok(Some(BatchReader { f, cur: None }))
    }

    fn close(&mut self, batch: usize, side: usize) -> Result<()> {
        if let Some(f) = self.batches.get_mut(batch).and_then(|b| b[side].take()) {
            // BufFileClose flushes even a skipped batch's buffered tail.
            f.acct.as_ref().unwrap().pg_size(f.bytes)?;
        }
        Ok(())
    }
}

fn push_chunk(chunks: &mut Vec<(u64, u64)>, (s, e): (u64, u64)) {
    match chunks.last_mut() {
        Some(last) if last.1 == s => last.1 = e,
        _ => chunks.push((s, e)),
    }
}

/// A batch file read back (ExecHashJoinGetSavedTuple): (hash value, t_len, row). Closed
/// (its PostgreSQL file reported) when dropped.
pub struct BatchReader {
    /// (Its chunks, the next one last.)
    f: BatchFile,
    cur: Option<RowReader>,
}

impl BatchReader {
    fn next(&mut self, files: &mut Files) -> Result<Option<(u32, u32, Row)>> {
        loop {
            if let Some(r) = &mut self.cur
                && let Some(([h, len], row)) = r.read_tagged_row()?
            {
                return Ok(Some((h, len, row)));
            }
            let Some((s, e)) = self.f.chunks.pop() else { return Ok(None) };
            let file = files.file.as_mut().ok_or_else(|| crate::error::internal("hash join batch file missing"))?;
            self.cur = Some(file.reader(s, e)?);
        }
    }
}

/// A Hash node's table (HashJoinTableData) and its batch files. `rows` are the batch's
/// inner rows in the order dense_alloc placed them since the table was last copied, which
/// is also the order they were pushed onto their buckets.
pub struct HashTable {
    pub nbuckets: u32,
    nbuckets_optimal: u32,
    pub nbuckets_original: u32,
    pub nbatch: u32,
    pub nbatch_original: u32,
    nbatch_outstart: u32,
    pub curbatch: u32,
    grow: bool,
    space_used: i64,
    space_allowed: i64,
    pub space_peak: i64,
    pub total_tuples: u64,
    pub rows: Vec<Row>,
    hashes: Vec<u32>,
    lens: Vec<u32>,
    /// The build's end made the buckets anew (ExecHashIncreaseNumBuckets).
    rebucketed: bool,
    files: Files,
    /// The skew optimization's MCVs it may use, and its table while batch 0 runs.
    skew_mcvs: u32,
    skew: Option<Skew>,
    space_used_skew: i64,
    space_allowed_skew: i64,
}

/// ExecHashBuildSkewHash's table: buckets for the outer column's most common values' hash
/// values (open addressing over `slots`), their inner tuples kept apart from the main
/// table in batch 0.
struct Skew {
    slots: Vec<Option<usize>>,
    /// The buckets in the order they were made (skewBucketNums): the last goes first.
    buckets: Vec<SkewBucket>,
}

struct SkewBucket {
    hash: u32,
    slot: usize,
    /// (row, t_len), in the order they came (the chain's head last).
    tuples: Vec<(Row, u32)>,
}

/// SKEW_BUCKET_OVERHEAD.
const SKEW_BUCKET_OVERHEAD: i64 = 16;

impl HashTable {
    /// ExecHashTableCreate over the planner's estimate of the inner side.
    pub fn new(rows: f64, width: u32, useskew: bool, ctx: &ExecCtx) -> HashTable {
        let hash_bytes = super::pghash::hash_mem(ctx).floor();
        let (nb, nbatch, space, skew_mcvs) = crate::sql::joins::choose_hash_table_size(rows, width, hash_bytes, useskew);
        Self::sized(nb, nbatch, space, skew_mcvs)
    }

    /// A Parallel Hash's table when the participants' combined hash_mem holds it whole
    /// (ExecChooseHashTableSize's try_combined_hash_mem: one batch); None when it doesn't.
    pub fn new_shared(ph: &crate::sql::explain::ParallelHash, ctx: &ExecCtx) -> Option<HashTable> {
        let g = crate::sql::joins::JoinGucs::read(ctx);
        let (nb, nbatch, space) = crate::sql::joins::parallel_hash_table_space(ph.rows_total, ph.width, &g, ph.workers);
        (nbatch == 1).then(|| Self::sized(nb, 1, space, 0))
    }

    fn sized(nb: u32, nbatch: u32, space: f64, skew_mcvs: u32) -> HashTable {
        HashTable {
            nbuckets: nb,
            nbuckets_optimal: nb,
            nbuckets_original: nb,
            nbatch,
            nbatch_original: nbatch,
            nbatch_outstart: nbatch,
            curbatch: 0,
            grow: true,
            space_used: 0,
            space_allowed: space as i64,
            space_peak: 0,
            total_tuples: 0,
            rows: Vec::new(),
            hashes: Vec::new(),
            lens: Vec::new(),
            rebucketed: false,
            files: Files::default(),
            skew_mcvs,
            skew: None,
            space_used_skew: 0,
            space_allowed_skew: space as i64 * 2 / 100,
        }
    }

    /// Whether ExecHashTableCreate would set up the skew table (more than one batch, MCVs
    /// to use): then `build_skew` with the outer column's MCVs.
    pub fn wants_skew(&self) -> bool {
        self.nbatch > 1 && self.skew_mcvs > 0
    }

    /// ExecHashBuildSkewHash over the outer column's MCVs: (their hash values, their
    /// frequencies), most common first.
    pub fn build_skew(&mut self, mcvs: &[(u32, f32)]) {
        let n = (self.skew_mcvs as usize).min(mcvs.len());
        if n == 0 || mcvs[..n].iter().map(|m| m.1 as f64).sum::<f64>() < 0.01 {
            return;
        }
        let len = ((n + 1).next_power_of_two() << 2) as u32;
        let mut s = Skew { slots: vec![None; len as usize], buckets: Vec::new() };
        let bytes = len as i64 * 8 + n as i64 * 4;
        self.space_used += bytes;
        self.space_used_skew += bytes;
        self.space_peak = self.space_peak.max(self.space_used);
        for &(h, _) in &mcvs[..n] {
            let mut b = (h & (len - 1)) as usize;
            while let Some(i) = s.slots[b]
                && s.buckets[i].hash != h
            {
                b = (b + 1) & (len as usize - 1);
            }
            if s.slots[b].is_some() {
                continue;
            }
            s.slots[b] = Some(s.buckets.len());
            s.buckets.push(SkewBucket { hash: h, slot: b, tuples: Vec::new() });
            self.space_used += SKEW_BUCKET_OVERHEAD;
            self.space_used_skew += SKEW_BUCKET_OVERHEAD;
            self.space_peak = self.space_peak.max(self.space_used);
        }
        self.skew = Some(s);
    }

    /// ExecHashGetSkewBucket: the skew bucket of hash value `h`.
    pub fn skew_bucket(&self, h: u32) -> Option<usize> {
        let s = self.skew.as_ref()?;
        let len = s.slots.len();
        let mut b = h as usize & (len - 1);
        while let Some(i) = s.slots[b] {
            if s.buckets[i].hash == h {
                return Some(i);
            }
            b = (b + 1) & (len - 1);
        }
        None
    }

    /// ExecHashSkewTableInsert.
    fn skew_insert(&mut self, i: usize, row: Row, tlen: i64, ctx: &ExecCtx) -> Result<()> {
        self.skew.as_mut().unwrap().buckets[i].tuples.push((row, tlen as u32));
        let size = HJTUPLE_OVERHEAD + tlen;
        self.space_used += size;
        self.space_used_skew += size;
        self.space_peak = self.space_peak.max(self.space_used);
        while self.space_used_skew > self.space_allowed_skew && self.skew.is_some() {
            self.remove_next_skew_bucket(ctx)?;
        }
        if self.space_used > self.space_allowed {
            self.increase_batches(ctx)?;
        }
        Ok(())
    }

    /// ExecHashRemoveNextSkewBucket: the last bucket made goes, its tuples (head first) to
    /// the main table or their batch's file.
    fn remove_next_skew_bucket(&mut self, ctx: &ExecCtx) -> Result<()> {
        let s = self.skew.as_mut().unwrap();
        let b = s.buckets.pop().unwrap();
        s.slots[b.slot] = None;
        let batch = self.batch_of(b.hash);
        for (row, len) in b.tuples.into_iter().rev() {
            let size = HJTUPLE_OVERHEAD + len as i64;
            if batch == self.curbatch {
                self.rows.push(row);
                self.hashes.push(b.hash);
                self.lens.push(len);
                self.space_used_skew -= size;
            } else {
                self.files.save(batch as usize, INNER, b.hash, &row, len as i64, ctx)?;
                self.space_used -= size;
                self.space_used_skew -= size;
            }
        }
        self.space_used -= SKEW_BUCKET_OVERHEAD;
        self.space_used_skew -= SKEW_BUCKET_OVERHEAD;
        if self.skew.as_ref().is_some_and(|s| s.buckets.is_empty()) {
            self.skew = None;
            self.space_used -= self.space_used_skew;
            self.space_used_skew = 0;
        }
        Ok(())
    }

    /// ExecHashGetBucketAndBatch's batch.
    pub fn batch_of(&self, hash: u32) -> u32 {
        if self.nbatch > 1 { hash.rotate_right(self.nbuckets.trailing_zeros()) & (self.nbatch - 1) } else { 0 }
    }

    /// MultiExecPrivateHash: an inner row (not one of a NULL key it drops).
    pub fn build_insert(&mut self, row: Row, hash: u32, tlen: i64, ctx: &ExecCtx) -> Result<()> {
        match self.skew_bucket(hash) {
            Some(i) => self.skew_insert(i, row, tlen, ctx)?,
            None => self.insert(row, hash, tlen, ctx)?,
        }
        self.total_tuples += 1;
        Ok(())
    }

    /// ExecHashTableInsert.
    fn insert(&mut self, row: Row, hash: u32, tlen: i64, ctx: &ExecCtx) -> Result<()> {
        let b = self.batch_of(hash);
        if b != self.curbatch {
            return self.files.save(b as usize, INNER, hash, &row, tlen, ctx);
        }
        // (NTUP_PER_BUCKET exceeded by the tuples before this one, while there is one batch.)
        if self.nbatch == 1
            && self.total_tuples > self.nbuckets_optimal as u64
            && self.nbuckets_optimal <= i32::MAX as u32 / 2
            && self.nbuckets_optimal as u64 * 2 <= MAX_POINTERS
        {
            self.nbuckets_optimal *= 2;
        }
        self.rows.push(row);
        self.hashes.push(hash);
        self.lens.push(tlen as u32);
        self.space_used += HJTUPLE_OVERHEAD + tlen;
        self.space_peak = self.space_peak.max(self.space_used);
        if self.space_used + self.nbuckets_optimal as i64 * 8 > self.space_allowed {
            self.increase_batches(ctx)?;
        }
        Ok(())
    }

    /// ExecHashIncreaseNumBatches (with ExecHashIncreaseBatchSize first).
    fn increase_batches(&mut self, ctx: &ExecCtx) -> Result<()> {
        if !self.grow || self.nbatch as u64 > (i32::MAX as u64 / 2).min(((1 << 30) - 1) / 16) {
            return Ok(());
        }
        // (Doubling the table's space costs less than the batch files' buffers would.)
        if self.space_allowed <= self.nbatch as i64 * 2 * BLCKSZ {
            self.space_allowed *= 2;
            return Ok(());
        }
        self.nbatch *= 2;
        self.nbuckets = self.nbuckets_optimal;
        let order = chunk_walk(&self.lens);
        let mut rows = std::mem::take(&mut self.rows);
        let hashes = std::mem::take(&mut self.hashes);
        let lens = std::mem::take(&mut self.lens);
        let mut freed = 0;
        for &i in &order {
            let (h, len) = (hashes[i], lens[i]);
            let row = std::mem::take(&mut rows[i]);
            let b = self.batch_of(h);
            if b == self.curbatch {
                self.rows.push(row);
                self.hashes.push(h);
                self.lens.push(len);
            } else {
                self.files.save(b as usize, INNER, h, &row, len as i64, ctx)?;
                self.space_used -= HJTUPLE_OVERHEAD + len as i64;
                freed += 1;
            }
        }
        // (All or none moved: tuples of one hash value, which more batches can't split.)
        if freed == 0 || freed == order.len() {
            self.grow = false;
        }
        Ok(())
    }

    /// The end of MultiExecPrivateHash: the buckets grown to their optimal count, and the
    /// bucket array counted.
    pub fn finish_build(&mut self) {
        if self.nbuckets != self.nbuckets_optimal {
            self.nbuckets = self.nbuckets_optimal;
            self.rebucketed = true;
        }
        self.space_used += self.nbuckets as i64 * 8;
        self.space_peak = self.space_peak.max(self.space_used);
        self.nbatch_outstart = self.nbatch;
    }

    /// Whether rows go to batch files (now or later batches are left).
    pub fn batched(&self) -> bool {
        self.nbatch > 1
    }

    /// The batch's rows (the main table's, then the skew buckets') and their bucket chains
    /// (the order of matches and of unmatched inner rows), taken out of the table.
    pub fn take_batch(&mut self) -> (Vec<Row>, JoinTable) {
        let seq: Vec<usize> = if self.rebucketed { chunk_walk(&self.lens) } else { (0..self.rows.len()).collect() };
        let mut table = JoinTable::flat(self.nbuckets, seq.into_iter().map(|i| (i, self.hashes[i])));
        let mut rows = std::mem::take(&mut self.rows);
        if let Some(s) = &mut self.skew {
            let mut chains = Vec::with_capacity(s.buckets.len());
            for b in &mut s.buckets {
                let start = rows.len();
                rows.extend(b.tuples.drain(..).map(|(r, _)| r));
                chains.push((start..rows.len()).rev().collect());
            }
            table = table.with_skew(chains);
        }
        (rows, table)
    }

    /// An outer row of a later batch, to that batch's outer file.
    pub fn save_outer(&mut self, batch: u32, hash: u32, row: &[Value], tlen: i64, ctx: &ExecCtx) -> Result<()> {
        self.files.save(batch as usize, OUTER, hash, row, tlen, ctx)
    }

    /// The next row of a batch file: (hash value, t_len, row).
    pub fn read(&mut self, r: &mut BatchReader) -> Result<Option<(u32, u32, Row)>> {
        r.next(&mut self.files)
    }

    /// ExecHashJoinNewBatch: the next batch with work, its inner rows loaded (None: no
    /// more), and its outer file to probe with.
    pub fn new_batch(&mut self, fill_outer: bool, fill_inner: bool, ctx: &ExecCtx) -> Result<Option<Option<BatchReader>>> {
        let nbatch = self.nbatch as usize;
        let mut cur = self.curbatch as usize;
        if cur > 0 {
            self.files.close(cur, OUTER)?;
        } else {
            // (The skew table is batch 0's only.)
            self.skew = None;
            self.space_used_skew = 0;
        }
        cur += 1;
        while cur < nbatch && (!self.files.has(cur, OUTER) || !self.files.has(cur, INNER)) {
            let (o, i) = (self.files.has(cur, OUTER), self.files.has(cur, INNER));
            if (o && fill_outer)
                || (i && fill_inner)
                || (i && nbatch != self.nbatch_original as usize)
                || (o && nbatch != self.nbatch_outstart as usize)
            {
                break;
            }
            self.files.close(cur, INNER)?;
            self.files.close(cur, OUTER)?;
            cur += 1;
        }
        if cur >= nbatch {
            return Ok(None);
        }
        self.curbatch = cur as u32;
        // ExecHashTableReset.
        self.rows.clear();
        self.hashes.clear();
        self.lens.clear();
        self.space_used = 0;
        self.rebucketed = false;
        if let Some(mut r) = self.files.take(cur, INNER, ctx)? {
            while let Some((h, len, row)) = r.next(&mut self.files)? {
                ctx.check()?;
                self.insert(row, h, len as i64, ctx)?;
            }
        }
        Ok(Some(self.files.take(cur, OUTER, ctx)?))
    }

    /// EXPLAIN ANALYZE's numbers (HashInstrumentation).
    pub fn stats(&self) -> super::HashStats {
        super::HashStats {
            nbuckets: self.nbuckets,
            nbuckets_original: self.nbuckets_original,
            nbatch: self.nbatch,
            nbatch_original: self.nbatch_original,
            space_peak: self.space_peak as u64,
            tuples: self.total_tuples,
        }
    }
}

/// The tuples of `lens` (t_len, in the order dense_alloc placed them) in the order a walk
/// of the table's chunks meets them: the newest chunk first, a tuple over the chunk
/// threshold in a chunk of its own second in the list.
fn chunk_walk(lens: &[u32]) -> Vec<usize> {
    // (The list's head last: each chunk's tuples and the room left in it.)
    let mut list: Vec<(Vec<u32>, i64)> = Vec::new();
    for (i, &len) in lens.iter().enumerate() {
        let size = super::workmem::maxalign(HJTUPLE_OVERHEAD + len as i64);
        if size > HASH_CHUNK_THRESHOLD {
            let at = list.len().saturating_sub(1);
            list.insert(at, (vec![i as u32], 0));
            continue;
        }
        match list.last_mut() {
            Some((ts, room)) if *room >= size => {
                ts.push(i as u32);
                *room -= size;
            }
            _ => list.push((vec![i as u32], HASH_CHUNK_SIZE - size)),
        }
    }
    list.iter().rev().flat_map(|(ts, _)| ts.iter().map(|&i| i as usize)).collect()
}
