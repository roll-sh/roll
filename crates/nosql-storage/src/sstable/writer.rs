//! Streaming SSTable writer: partitions go in in token order, blocks go out to a `Write` as they
//! fill. Only the block index, the bloom filter and statistics stay in memory.

use super::bloom::Bloom;
use super::stats::SstStats;
use super::{BlockMeta, Codec, Continuation, E_END, E_MARKER, E_PARTITION, E_ROW, Index};
use crate::codec::{put_bound, put_bytes, put_deletion, put_row, put_uvarint};
use crate::compare::{ClusteringComparator, Pos};
use crate::model::*;
use crate::tombstones::Marker;
use anyhow::{Result, bail};
use std::io::Write;
use std::sync::Arc;

pub const DEFAULT_BLOCK_SIZE: usize = 64 << 10;

#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub block_size: usize,
    pub codec: Codec,
    pub bloom_fp_chance: f64,
    /// For sizing the bloom filter.
    pub expected_partitions: u64,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self { block_size: DEFAULT_BLOCK_SIZE, codec: Codec::default(), bloom_fp_chance: 0.01, expected_partitions: 1024 }
    }
}

struct Open {
    key: PartitionKey,
    deletion: DeletionTime,
    static_row: Option<Row>,
    /// The header is written lazily, so a partition that ends up empty is dropped.
    written: bool,
    open: Option<DeletionTime>,
    start_bytes: u64,
}

pub struct SstWriter<W: Write> {
    out: W,
    cmp: Arc<dyn ClusteringComparator>,
    opts: WriterOptions,
    buf: Vec<u8>,
    /// Index entry of the block being filled (set by its first entry).
    pending: Option<BlockMeta>,
    offset: u64,
    blocks: Vec<BlockMeta>,
    bloom: Bloom,
    stats: SstStats,
    part: Option<Open>,
    last_key: Option<PartitionKey>,
    raw_total: u64,
    /// Offsets of the partition headers in the block being filled.
    headers: Vec<u32>,
}

impl<W: Write> SstWriter<W> {
    pub fn new(out: W, cmp: Arc<dyn ClusteringComparator>, opts: WriterOptions) -> Self {
        let bloom = Bloom::new(opts.expected_partitions, opts.bloom_fp_chance);
        Self {
            out,
            cmp,
            buf: Vec::with_capacity(opts.block_size + (opts.block_size >> 2)),
            opts,
            pending: None,
            offset: 0,
            blocks: Vec::new(),
            bloom,
            stats: SstStats::default(),
            part: None,
            last_key: None,
            raw_total: 0,
            headers: Vec::new(),
        }
    }

    /// Bytes written so far (compressed), plus the block being filled.
    pub fn bytes_written(&self) -> u64 {
        self.offset + self.buf.len() as u64
    }

    pub fn partitions(&self) -> u64 {
        self.stats.partitions
    }

    pub fn is_empty(&self) -> bool {
        self.stats.partitions == 0
    }

    pub fn begin_partition(&mut self, key: &PartitionKey, deletion: DeletionTime, static_row: Option<Row>) -> Result<()> {
        if self.part.is_some() {
            bail!("sstable writer: partition not ended");
        }
        if let Some(last) = &self.last_key
            && key <= last
        {
            bail!("sstable writer: partitions out of order");
        }
        let static_row = static_row.filter(|r| !r.is_empty());
        self.part = Some(Open { key: key.clone(), deletion, static_row, written: false, open: None, start_bytes: 0 });
        Ok(())
    }

    fn write_header(&mut self) -> Result<()> {
        let Some(p) = self.part.as_mut() else { bail!("sstable writer: no partition") };
        if p.written {
            return Ok(());
        }
        p.written = true;
        let (key, deletion, static_row) = (p.key.clone(), p.deletion, p.static_row.take());
        self.last_key = Some(key.clone());
        self.bloom.insert(&key.key);
        self.stats.add_partition(&key, &deletion);
        if let Some(s) = &static_row {
            self.stats.add_row(s, true);
        }
        self.start_entry(None)?;
        let p = self.part.as_mut().expect("open");
        p.start_bytes = self.raw_total + self.buf.len() as u64;
        self.headers.push(self.buf.len() as u32);
        self.buf.push(E_PARTITION);
        self.buf.extend_from_slice(&key.token.to_be_bytes());
        put_bytes(&mut self.buf, &key.key);
        let flags = (!deletion.is_live()) as u8 | ((static_row.is_some() as u8) << 1);
        self.buf.push(flags);
        if !deletion.is_live() {
            put_deletion(&mut self.buf, &deletion);
        }
        if let Some(s) = &static_row {
            put_row(&mut self.buf, s, false);
        }
        Ok(())
    }

    /// Record the index entry of a new block when its first entry is written; `first` is the
    /// position of a non-header entry.
    fn start_entry(&mut self, first: Option<Bound>) -> Result<()> {
        // Blocks are cut before an entry, never between a partition's last entry and its end.
        if self.buf.len() >= self.opts.block_size {
            self.cut()?;
        }
        if self.pending.is_some() {
            return Ok(());
        }
        let p = self.part.as_ref().expect("open partition");
        let cont = first.map(|first| Continuation { first, open: p.open, partition_deletion: p.deletion });
        self.pending = Some(BlockMeta { offset: self.offset, clen: 0, rlen: 0, key: p.key.clone(), cont });
        Ok(())
    }

    pub fn add_row(&mut self, row: &Row) -> Result<()> {
        if row.is_empty() {
            return Ok(());
        }
        self.write_header()?;
        self.start_entry(Some(Bound::inclusive_start(row.clustering.clone())))?;
        self.stats.add_row(row, false);
        self.stats.clustering(&*self.cmp, Pos::Row(&row.clustering));
        let mut body = Vec::with_capacity(64);
        put_row(&mut body, row, true);
        self.buf.push(E_ROW);
        put_uvarint(&mut self.buf, body.len() as u64);
        self.buf.extend_from_slice(&body);
        Ok(())
    }

    pub fn add_marker(&mut self, m: &Marker) -> Result<()> {
        if m.open.is_none() && m.close.is_none() {
            return Ok(());
        }
        self.write_header()?;
        self.start_entry(Some(m.bound.clone()))?;
        self.stats.add_marker(m);
        self.stats.clustering(&*self.cmp, Pos::Bound(&m.bound));
        self.buf.push(E_MARKER);
        put_bound(&mut self.buf, &m.bound);
        self.buf.push(m.close.is_some() as u8 | ((m.open.is_some() as u8) << 1));
        if let Some(d) = &m.close {
            put_deletion(&mut self.buf, d);
        }
        if let Some(d) = &m.open {
            put_deletion(&mut self.buf, d);
        }
        self.part.as_mut().expect("open").open = m.open;
        Ok(())
    }

    pub fn end_partition(&mut self) -> Result<()> {
        let Some(p) = self.part.as_ref() else { bail!("sstable writer: no partition") };
        if !p.written && (!p.deletion.is_live() || p.static_row.is_some()) {
            self.write_header()?;
        }
        let p = self.part.take().expect("open");
        if !p.written {
            return Ok(());
        }
        if p.open.is_some() {
            bail!("sstable writer: partition ended with an open range tombstone");
        }
        self.buf.push(E_END);
        let size = self.raw_total + self.buf.len() as u64 - p.start_bytes;
        self.stats.max_partition_bytes = self.stats.max_partition_bytes.max(size);
        Ok(())
    }

    fn cut(&mut self) -> Result<()> {
        let Some(mut meta) = self.pending.take() else { return Ok(()) };
        // Trailer: where the partition headers are, for binary search within the block.
        let entries = self.buf.len() as u64;
        for o in self.headers.drain(..) {
            self.buf.extend_from_slice(&o.to_le_bytes());
        }
        let n = ((self.buf.len() as u64 - entries) / 4) as u32;
        self.buf.extend_from_slice(&n.to_le_bytes());
        let mut c = self.opts.codec.compress(&self.buf)?;
        let crc = crc32c::crc32c(&c);
        meta.clen = c.len() as u32;
        meta.rlen = self.buf.len() as u32;
        c.extend_from_slice(&crc.to_le_bytes());
        self.out.write_all(&c)?;
        self.offset += c.len() as u64;
        self.raw_total += entries;
        self.buf.clear();
        // The index outlives the data its keys were read from (decoded blocks of compaction
        // inputs, memtable frames): keep copies, not slices that would pin that data.
        meta.key = meta.key.detached();
        if let Some(c) = &mut meta.cont {
            c.first = c.first.detached();
        }
        self.blocks.push(meta);
        Ok(())
    }

    /// Give up: the output as written so far (to delete it).
    pub fn abandon(self) -> W {
        self.out
    }

    /// Finish: flush the last block, return the output and the index.
    pub fn finish(mut self) -> Result<(W, Index)> {
        if self.part.is_some() {
            bail!("sstable writer: partition not ended");
        }
        self.cut()?;
        self.out.flush()?;
        self.stats.uncompressed_bytes = self.raw_total;
        let st = &mut self.stats;
        st.first_key = st.first_key.detached();
        st.last_key = st.last_key.detached();
        st.min_clustering = st.min_clustering.as_ref().map(Bound::detached);
        st.max_clustering = st.max_clustering.as_ref().map(Bound::detached);
        let index = Index { codec: self.opts.codec, blocks: self.blocks, bloom: self.bloom, stats: self.stats };
        Ok((self.out, index))
    }
}
