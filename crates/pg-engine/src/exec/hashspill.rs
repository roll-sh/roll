//! A HashAggregate past hash_mem: nodeAgg's spilling (PostgreSQL 13+). Groups are counted
//! as nodeAgg counts them (`workmem::HashAggMem`); once a new group takes the table past
//! its limits the node goes into spill mode: rows of groups already in the table are still
//! aggregated, the others are written to partitions chosen by bits of their key's hash.
//! When the input ends, the table's groups come out, then each partition in turn (the last
//! made first) is aggregated afresh as a batch, which may spill again into partitions of
//! the next hash bits. Partition counts, cardinality estimates (HyperLogLog) and limits are
//! nodeAgg's, so the groups come out in its order and EXPLAIN ANALYZE shows its batches.

use super::spill::{RowReader, TempFile};
use super::workmem::{self, HashAggMem};
use super::{ExecCtx, Row};
use crate::error::Result;
use crate::types::{DataType, Value};

/// HASHAGG_READ_BUFFER_SIZE / HASHAGG_WRITE_BUFFER_SIZE, the HyperLogLog width, partitions.
const BUF: i64 = 8192;
const HLL_BITS: u32 = 5;
/// The engine's write buffer per partition.
const PART_BUF: usize = 16 << 10;

/// PostgreSQL's hyperloglog.c (initHyperLogLog of width 5, addHyperLogLog, estimate).
#[derive(Clone, Default)]
struct Hll([u8; 32]);

impl Hll {
    fn add(&mut self, hash: u32) {
        let index = (hash >> (32 - HLL_BITS)) as usize;
        let x = hash << HLL_BITS;
        let b = 32 - HLL_BITS;
        let rho = if x == 0 { b + 1 } else { (x.leading_zeros() + 1).min(b + 1) };
        self.0[index] = self.0[index].max(rho as u8);
    }

    fn estimate(&self) -> f64 {
        let m = 32.0;
        let sum: f64 = self.0.iter().map(|&r| 1.0 / 2f64.powi(r as i32)).sum();
        let mut result = 0.697 * m * m / sum;
        let p32 = 4294967296.0;
        if result <= 2.5 * m {
            let zeros = self.0.iter().filter(|&&r| r == 0).count();
            if zeros != 0 {
                result = m * (m / zeros as f64).ln();
            }
        } else if result > p32 / 30.0 {
            result = -p32 * (1.0 - result / p32).ln();
        }
        result
    }
}

struct Part {
    chunks: Vec<(u64, u64)>,
    buf: Vec<u8>,
    ntuples: u64,
    hll: Hll,
    /// PostgreSQL's bytes on its tape, and the tape's blocks.
    pg: i64,
    tape: workmem::Tape,
}

/// A partition's rows, read back as a batch: its chunks in order.
pub struct Batch {
    chunks: std::collections::VecDeque<(u64, u64)>,
    cur: Option<RowReader>,
    used_bits: u32,
    input_card: f64,
    pg: i64,
    tape: workmem::Tape,
}

pub struct HashSpill {
    pub mem: HashAggMem,
    /// Groups not in the table go to the partitions (hash_spill_mode).
    pub spilling: bool,
    file: Option<TempFile>,
    parts: Vec<Part>,
    shift: u32,
    mask: u32,
    /// The batches left, the last one next (hash_batches).
    batches: Vec<Batch>,
    pub batches_used: u64,
    /// Of the batch running: its hash bits used and its estimated groups.
    used_bits: u32,
    input_card: f64,
    pub entry_size: f64,
    hash_mem: f64,
    /// The input's columns the aggregation reads (the others go to disk as NULLs), and
    /// their types.
    need: Vec<bool>,
    types: Vec<DataType>,
    /// The node's tape set (its block numbers: the file's size), and the most of it.
    tapes: workmem::TapeSet,
    pub disk_peak: i64,
}

impl HashSpill {
    pub fn new(mem: HashAggMem, entry_size: f64, groups: f64, hash_mem: f64, need: Vec<bool>, types: Vec<DataType>) -> HashSpill {
        let mut s = HashSpill {
            mem,
            spilling: false,
            file: None,
            parts: Vec::new(),
            shift: 32,
            mask: 0,
            batches: Vec::new(),
            batches_used: 1,
            used_bits: 0,
            input_card: groups,
            entry_size,
            hash_mem,
            need,
            types,
            tapes: workmem::TapeSet::default(),
            disk_peak: 0,
        };
        s.set_limits();
        s
    }

    /// hash_choose_num_partitions: (partitions, their bits).
    fn num_partitions(&self, groups: f64) -> (usize, u32) {
        let limit = (self.hash_mem * 0.25 - BUF as f64) / BUF as f64;
        let mut d = 1.0 + 1.5 * groups * self.entry_size / self.hash_mem;
        if d > limit {
            d = limit;
        }
        let n = d.clamp(4.0, 1024.0) as u32;
        let mut bits = 31 - n.leading_zeros();
        if n > 1 << bits {
            bits += 1;
        }
        // (my_log2 rounds up.)
        if bits + self.used_bits >= 32 {
            bits = 32 - self.used_bits;
        }
        (1 << bits, bits)
    }

    /// hash_agg_set_limits for the batch about to run.
    fn set_limits(&mut self) {
        let hm = self.hash_mem;
        let e = self.entry_size;
        if self.input_card * e <= hm {
            self.mem.mem_limit = hm as i64;
            self.mem.ngroups_limit = (hm / e) as u64;
            return;
        }
        let (n, _) = self.num_partitions(self.input_card);
        let partition_mem = (BUF + BUF * n as i64) as f64;
        let limit = if hm > 4.0 * partition_mem { hm - partition_mem } else { hm * 0.75 };
        self.mem.mem_limit = limit as i64;
        self.mem.ngroups_limit = if limit > e { (limit / e) as u64 } else { 1 };
    }

    /// A new group in the table (its key tuple `tuple_len` bytes, the table `slots` big):
    /// whether the node goes into spill mode.
    pub fn new_group(&mut self, tuple_len: i64, slots: u64) {
        self.mem.table(slots);
        if self.mem.new_group(tuple_len) {
            self.spilling = true;
        }
    }

    /// hashagg_spill_tuple: a row of a group not in the table, its key hashing to `hash`.
    pub fn spill(&mut self, hash: u32, row: &Row, ctx: &ExecCtx) -> Result<()> {
        if self.parts.is_empty() {
            // hashagg_spill_init (for the first pass, at hash_agg_enter_spill_mode).
            let (n, bits) = self.num_partitions(self.input_card);
            self.shift = 32 - self.used_bits - bits;
            self.mask = if self.shift < 32 { ((n as u32) - 1) << self.shift } else { 0 };
            self.parts = (0..n)
                .map(|_| Part {
                    chunks: Vec::new(),
                    buf: Vec::new(),
                    ntuples: 0,
                    hll: Hll::default(),
                    pg: 0,
                    tape: workmem::Tape::default(),
                })
                .collect();
            if self.file.is_none() {
                self.file = Some(TempFile::create(ctx)?);
            }
        }
        let p = if self.shift < 32 { ((hash & self.mask) >> self.shift) as usize } else { 0 };
        // (Columns past the mask's end aren't read either.)
        let masked: Row = row
            .iter()
            .enumerate()
            .map(|(i, v)| if self.need.get(i).copied().unwrap_or(false) { v.clone() } else { Value::Null })
            .collect();
        let pg = 4 + workmem::minimal_tuple_len(&masked, &self.types, ctx);
        let part = &mut self.parts[p];
        part.ntuples += 1;
        part.hll.add(crate::catalog::hashidx::hash_uint32(hash));
        part.pg += pg;
        super::spill::put_tagged_row(&mut part.buf, [pg as u32, 0], &masked);
        self.tapes.write(&mut part.tape, pg);
        let file = self.file.as_mut().unwrap();
        if part.buf.len() >= PART_BUF {
            let c = file.append(&part.buf)?;
            part.chunks.push(c);
            part.buf.clear();
        }
        file.pg_size((self.tapes.written * workmem::BLCKSZ) as u64)
    }

    /// The input (or a batch) ended: its partitions become batches (hashagg_spill_finish)
    /// and the metrics are updated; the table is emptied for what comes next.
    pub fn end_input(&mut self, from_tape: bool) -> Result<()> {
        let nparts = self.parts.len() as i64;
        let used_bits = 32 - self.shift;
        for mut part in std::mem::take(&mut self.parts) {
            if part.ntuples == 0 {
                continue;
            }
            if !part.buf.is_empty() {
                let c = self.file.as_mut().unwrap().append(&part.buf)?;
                part.chunks.push(c);
            }
            self.tapes.rewind(&mut part.tape);
            self.batches.push(Batch {
                chunks: part.chunks.into(),
                cur: None,
                used_bits,
                input_card: part.hll.estimate(),
                pg: part.pg,
                tape: part.tape,
            });
            self.batches_used += 1;
        }
        let buffers = nparts * BUF + if from_tape { BUF } else { 0 };
        if let Some(e) = self.mem.update(buffers) {
            self.entry_size = e;
        }
        let blocks = self.tapes.written;
        self.disk_peak = self.disk_peak.max(blocks * workmem::BLCKSZ / 1024);
        if let Some(f) = &self.file {
            f.pg_size((blocks * workmem::BLCKSZ) as u64)?;
        }
        self.spilling = false;
        Ok(())
    }

    /// The next batch to aggregate (agg_refill_hash_table): the table reset, its limits set.
    pub fn next_batch(&mut self) -> Option<Batch> {
        let b = self.batches.pop()?;
        self.used_bits = b.used_bits;
        self.input_card = b.input_card;
        self.set_limits();
        self.mem.reset();
        if let Some(f) = &self.file {
            f.pg_read(b.pg as u64);
        }
        Some(b)
    }

    /// The next row of batch `b` (its tape's blocks freed as they are read).
    pub fn read(&mut self, b: &mut Batch) -> Result<Option<Row>> {
        loop {
            if let Some(r) = &mut b.cur
                && let Some(([pg, _], row)) = r.read_tagged_row()?
            {
                self.tapes.read(&mut b.tape, pg as i64);
                return Ok(Some(row));
            }
            let Some((s, e)) = b.chunks.pop_front() else { return Ok(None) };
            b.cur = Some(self.file.as_mut().unwrap().reader(s, e)?);
        }
    }

    /// EXPLAIN ANALYZE's "Batches: N  Memory Usage: NkB  Disk Usage: NkB".
    pub fn stats(&self) -> (u64, u64, u64) {
        // (The meta context's second block, which the table's own structures take, isn't in
        // the limits' model: PostgreSQL 18.6 shows 32 kB for a table of a few groups and 8 kB
        // more than the model's peak for bigger ones without spilling.)
        let peak = self.mem.peak.max(self.mem.total()) + 8192;
        (self.batches_used, (peak as u64).div_ceil(1024), self.disk_peak as u64)
    }
}
