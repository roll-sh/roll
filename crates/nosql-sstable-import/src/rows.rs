//! Partitions of a `Data.db`, as roll's storage model: a port of Cassandra's
//! `UnfilteredSerializer` (rows, static rows, range tombstone markers), `Cell.Serializer`,
//! `ClusteringPrefix.Serializer`, `ClusteringBoundOrBoundary.Serializer`,
//! `Columns.Serializer.deserializeSubset`, `DeletionTime.Serializer` / `LegacySerializer` and
//! `SerializationHeader`'s delta decoding, plus ScyllaDB's shadowable-tombstone extension flag
//! (documented in its SSTable 3 data file format).
//!
//! ```text
//! partition := be16 key-length key deletion-time [static-row] unfiltered* end(0x01)
//! unfiltered := flags [extended-flags] (clustering row-body | bound marker-body)
//! ```
//!
//! Column ids in the rows returned are positions in the serialization header's static or
//! regular columns ([`crate::stats::Header::columns`]); counter cells carry their raw counter
//! context. [`crate::load`] maps both to the target table.

use crate::descriptor::Version;
use crate::input::In;
use crate::stats::{Header, HeaderColumn};
use anyhow::{Result, bail};
use bytes::Bytes;
use nosql_storage::tombstones::Marker;
use nosql_storage::{Bound, Cell, Clustering, ColumnData, DeletionTime, LivenessInfo, NO_DELETION_TIME, NO_TTL, Row, RowDeletion};
use std::sync::Arc;

const END_OF_PARTITION: u8 = 0x01;
const IS_MARKER: u8 = 0x02;
const HAS_TIMESTAMP: u8 = 0x04;
const HAS_TTL: u8 = 0x08;
const HAS_DELETION: u8 = 0x10;
const HAS_ALL_COLUMNS: u8 = 0x20;
const HAS_COMPLEX_DELETION: u8 = 0x40;
const EXTENSION_FLAG: u8 = 0x80;

const IS_STATIC: u8 = 0x01;
const HAS_SHADOWABLE_DELETION: u8 = 0x02;
/// ScyllaDB: a shadowable tombstone follows the row's regular one.
const HAS_SCYLLA_SHADOWABLE_DELETION: u8 = 0x80;

const IS_DELETED: u8 = 0x01;
const IS_EXPIRING: u8 = 0x02;
const HAS_EMPTY_VALUE: u8 = 0x04;
const USE_ROW_TIMESTAMP: u8 = 0x08;
const USE_ROW_TTL: u8 = 0x10;

/// `Cell.INVALID_DELETION_TIME`: what a corrupt or overflowed deletion time decodes to.
pub const INVALID_DELETION_TIME: i64 = u32::MAX as i64 - 1;
/// `Cell.MAX_DELETION_TIME`.
const MAX_DELETION_TIME: i64 = u32::MAX as i64 - 2;
/// Largest row or marker body accepted (one row is held in memory at a time).
pub const MAX_ROW_BYTES: u64 = 256 << 20;

/// A partition's header.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionHeader {
    /// The serialised partition key (a composite key's `CompositeType` encoding).
    pub key: Bytes,
    pub deletion: DeletionTime,
    pub static_row: Option<Row>,
}

/// A row or a range tombstone marker.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Row(Row),
    Marker(Marker),
}

struct Col {
    fixed: Option<usize>,
    complex: bool,
}

/// Reads partitions one after the other.
pub struct Scanner<R: In> {
    pub(crate) r: R,
    header: Arc<Header>,
    /// Negative local deletion times read as unsigned (`hasUIntDeletionTime`, messaging 5.0).
    v50: bool,
    uint_partition_deletion: bool,
    clustering: Vec<Option<usize>>,
    statics: Vec<Col>,
    regulars: Vec<Col>,
    in_partition: bool,
    pending: Option<Item>,
    pending_end: bool,
}

fn cols(cs: &[HeaderColumn]) -> Vec<Col> {
    cs.iter().map(|c| Col { fixed: if c.complex { None } else { c.ty.fixed_len() }, complex: c.complex }).collect()
}

impl<R: In> Scanner<R> {
    pub fn new(r: R, header: Arc<Header>, version: &Version) -> Self {
        let clustering = header.clustering.iter().map(|t| t.fixed_len()).collect();
        let statics = cols(&header.statics);
        let regulars = cols(&header.regulars);
        Self { r, v50: version.uint_deletion_time(), uint_partition_deletion: version.uint_deletion_time(), header, clustering, statics, regulars, in_partition: false, pending: None, pending_end: false }
    }

    pub fn into_inner(self) -> R {
        self.r
    }

    pub fn input(&mut self) -> &mut R {
        &mut self.r
    }

    /// The next partition (`None` at the end of the file). Whatever is left of the current
    /// one is skipped.
    pub fn next_partition(&mut self) -> Result<Option<PartitionHeader>> {
        while self.in_partition {
            self.next_item()?;
        }
        if self.r.remaining() == 0 {
            return Ok(None);
        }
        let key = self.r.short_bytes()?;
        let deletion = self.partition_deletion()?;
        self.in_partition = true;
        let mut static_row = None;
        {
            let flags = self.r.u8()?;
            if flags & END_OF_PARTITION != 0 {
                self.pending_end = true;
            } else {
                let ext = if flags & EXTENSION_FLAG != 0 { self.r.u8()? } else { 0 };
                if flags & IS_MARKER == 0 && ext & IS_STATIC != 0 {
                    if self.header.statics.is_empty() {
                        bail!("corrupt data: a static row in a table without static columns");
                    }
                    let row = self.row_body(flags, ext, Clustering::EMPTY, true)?;
                    static_row = Some(row).filter(|r| !r.is_empty());
                } else {
                    self.pending = self.unfiltered(flags, ext)?;
                }
            }
        }
        Ok(Some(PartitionHeader { key, deletion, static_row }))
    }

    /// The next row or marker of the current partition (`None` at its end).
    pub fn next_item(&mut self) -> Result<Option<Item>> {
        if !self.in_partition {
            return Ok(None);
        }
        if let Some(p) = self.pending.take() {
            return Ok(Some(p));
        }
        loop {
            if self.pending_end {
                self.pending_end = false;
                self.in_partition = false;
                return Ok(None);
            }
            let flags = self.r.u8()?;
            if flags & END_OF_PARTITION != 0 {
                self.in_partition = false;
                return Ok(None);
            }
            let ext = if flags & EXTENSION_FLAG != 0 { self.r.u8()? } else { 0 };
            if flags & IS_MARKER == 0 && ext & IS_STATIC != 0 {
                bail!("corrupt data: a static row after the first row of a partition");
            }
            if let Some(item) = self.unfiltered(flags, ext)? {
                return Ok(Some(item));
            }
        }
    }

    /// One row or marker whose flags were read (`None`: an empty row, skipped as Cassandra
    /// does).
    fn unfiltered(&mut self, flags: u8, ext: u8) -> Result<Option<Item>> {
        if flags & IS_MARKER != 0 {
            return Ok(Some(Item::Marker(self.marker()?)));
        }
        let n = self.clustering.len();
        let values = self.clustering_values(n)?;
        let clustering = Clustering::new(values.iter().map(|v| v.as_deref().unwrap_or(&[])));
        let row = self.row_body(flags, ext, clustering, false)?;
        Ok((!row.is_empty()).then_some(Item::Row(row)))
    }

    fn clustering_values(&mut self, size: usize) -> Result<Vec<Option<Bytes>>> {
        if size > self.clustering.len() {
            bail!("corrupt data: a clustering prefix of {size} values for {} clustering columns", self.clustering.len());
        }
        let mut out = Vec::with_capacity(size);
        let mut offset = 0;
        while offset < size {
            let header = self.r.uvint()?;
            let limit = size.min(offset + 32);
            while offset < limit {
                // `ClusteringPrefix.Serializer.isNull/isEmpty` (Java shifts mod 64).
                let null = header & (1u64 << ((offset * 2 + 1) & 63)) != 0;
                let empty = header & (1u64 << ((offset * 2) & 63)) != 0;
                out.push(if null {
                    None
                } else if empty {
                    Some(Bytes::new())
                } else {
                    Some(self.value(self.clustering[offset])?)
                });
                offset += 1;
            }
        }
        Ok(out)
    }

    fn value(&mut self, fixed: Option<usize>) -> Result<Bytes> {
        match fixed {
            Some(n) => self.r.bytes(n),
            None => {
                let n = self.r.uvint32()?;
                if n < 0 {
                    bail!("corrupt data: negative value length");
                }
                self.r.bytes(n as usize)
            }
        }
    }

    fn timestamp(&mut self) -> Result<i64> {
        Ok((self.r.uvint()? as i64).wrapping_add(self.header.stats.min_timestamp))
    }

    fn ttl(&mut self) -> Result<i32> {
        Ok(self.r.uvint32()?.wrapping_add(self.header.stats.min_ttl))
    }

    fn local_deletion_time(&mut self) -> Result<i64> {
        Ok(self.r.uvint32()? as i64 + self.header.stats.min_local_deletion_time)
    }

    /// `Cell.decodeLocalDeletionTime`.
    fn decode_ldt(&self, ldt: i64, ttl: i32) -> i64 {
        if ldt >= ttl as i64 {
            return ldt;
        }
        if ldt < 0 {
            return if self.v50 { unsigned_deletion_time(ldt as i32) } else { INVALID_DELETION_TIME };
        }
        if ttl == nosql_storage::EXPIRED_LIVENESS_TTL { ldt } else { INVALID_DELETION_TIME }
    }

    /// A delta-encoded `DeletionTime` (`DeletionTime.build`).
    fn delta_deletion(&mut self) -> Result<DeletionTime> {
        let marked_at = self.timestamp()?;
        let mut ldt = self.local_deletion_time()?;
        if ldt < 0 {
            ldt = if self.v50 { unsigned_deletion_time(ldt as i32) } else { INVALID_DELETION_TIME };
        } else if ldt > MAX_DELETION_TIME {
            ldt = INVALID_DELETION_TIME;
        }
        Ok(DeletionTime::new(marked_at, ldt))
    }

    /// The partition-level deletion: legacy `(be32 ldt, be64 mfda)`, or since `oa` a single
    /// `0x80` for live and `(be64 mfda, be32 unsigned ldt)` otherwise.
    fn partition_deletion(&mut self) -> Result<DeletionTime> {
        if self.uint_partition_deletion {
            let first = self.r.u8()?;
            if first & 0x80 != 0 {
                if first != 0x80 {
                    bail!("corrupt data: deletion time flags {first:#04x}");
                }
                return Ok(DeletionTime::LIVE);
            }
            let rest = self.r.bytes(7)?;
            let mut mfda = [0u8; 8];
            mfda[0] = first;
            mfda[1..].copy_from_slice(&rest);
            let ldt = self.r.be32()? as i32;
            return Ok(DeletionTime::new(i64::from_be_bytes(mfda), unsigned_deletion_time(ldt)));
        }
        let ldt = self.r.be32()? as i32;
        let mfda = self.r.be64()? as i64;
        if mfda == i64::MIN && ldt == i32::MAX {
            return Ok(DeletionTime::LIVE);
        }
        Ok(DeletionTime::new(mfda, if ldt < 0 { INVALID_DELETION_TIME } else { ldt as i64 }))
    }

    /// Row size and previous-unfiltered size; returns the body length.
    fn sizes(&mut self) -> Result<u64> {
        let size = self.r.uvint()?;
        let before = self.r.remaining();
        self.r.uvint()?;
        let prev_len = before - self.r.remaining();
        let body = size.checked_sub(prev_len).ok_or_else(|| anyhow::anyhow!("corrupt data: row size {size}"))?;
        if body > self.r.remaining() || body > MAX_ROW_BYTES {
            bail!("corrupt data: a row of {body} bytes with {} left", self.r.remaining());
        }
        Ok(body)
    }

    fn row_body(&mut self, flags: u8, ext: u8, clustering: Clustering, is_static: bool) -> Result<Row> {
        let body = self.sizes()?;
        let start = self.r.remaining();
        let mut liveness = LivenessInfo::EMPTY;
        if flags & HAS_TIMESTAMP != 0 {
            let ts = self.timestamp()?;
            let (ttl, ldt) = if flags & HAS_TTL != 0 {
                let ttl = self.ttl()?;
                let ldt = self.local_deletion_time()?;
                (ttl, ldt)
            } else {
                (NO_TTL, NO_DELETION_TIME)
            };
            if ttl < 0 {
                bail!("corrupt data: TTL {ttl}");
            }
            let ldt = self.decode_ldt(ldt, ttl);
            liveness = if ttl == NO_TTL { LivenessInfo::live(ts) } else { LivenessInfo::expiring(ts, ttl, ldt) };
        }
        let mut deletion = RowDeletion::LIVE;
        if flags & HAS_DELETION != 0 {
            let d = self.delta_deletion()?;
            deletion = if ext & HAS_SHADOWABLE_DELETION != 0 { RowDeletion::shadowable(d) } else { RowDeletion::regular(d) };
        }
        if ext & HAS_SCYLLA_SHADOWABLE_DELETION != 0 {
            let s = self.delta_deletion()?;
            if s.supersedes(&deletion.time) {
                deletion = RowDeletion::shadowable(s);
            }
        }
        let superset = if is_static { self.statics.len() } else { self.regulars.len() };
        let present = if flags & HAS_ALL_COLUMNS != 0 { (0..superset).collect() } else { self.subset(superset)? };
        let mut columns = Vec::with_capacity(present.len());
        for i in present {
            let (complex, fixed) = {
                let c = if is_static { &self.statics[i] } else { &self.regulars[i] };
                (c.complex, c.fixed)
            };
            if complex {
                let mut cdel = DeletionTime::LIVE;
                if flags & HAS_COMPLEX_DELETION != 0 {
                    cdel = self.delta_deletion()?;
                }
                let n = self.r.uvint32()?;
                if n < 0 || n as u64 > self.r.remaining() {
                    bail!("corrupt data: {n} cells in a complex column");
                }
                let mut cells = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    cells.push(self.cell(&liveness, None, true)?);
                }
                columns.push(ColumnData::Complex { column: i as u32, deletion: cdel, cells });
            } else {
                let cell = self.cell(&liveness, fixed, false)?;
                columns.push(ColumnData::Simple { column: i as u32, cell });
            }
        }
        let used = start - self.r.remaining();
        if used > body {
            bail!("corrupt data: a row body overruns its size ({used} > {body})");
        }
        self.r.skip(body - used)?;
        Ok(Row { clustering, liveness, deletion, columns })
    }

    /// `Columns.Serializer.deserializeSubset`: the present columns' positions.
    fn subset(&mut self, superset: usize) -> Result<Vec<usize>> {
        let encoded = self.r.uvint()?;
        if encoded == 0 {
            return Ok((0..superset).collect());
        }
        if superset < 64 {
            if superset < 64 && encoded >> superset != 0 {
                bail!("corrupt data: column subset {encoded:#x} of {superset} columns");
            }
            return Ok((0..superset).filter(|i| encoded & (1 << i) == 0).collect());
        }
        let delta = usize::try_from(encoded).ok().filter(|d| *d <= superset).ok_or_else(|| anyhow::anyhow!("corrupt data: column subset delta {encoded}"))?;
        let count = superset - delta;
        let mut out = Vec::with_capacity(count);
        let idx = |r: &mut R| -> Result<usize> {
            let i = r.uvint32()?;
            usize::try_from(i).ok().filter(|i| *i <= superset).ok_or_else(|| anyhow::anyhow!("corrupt data: column index {i}"))
        };
        if count < superset / 2 {
            for _ in 0..count {
                let i = idx(&mut self.r)?;
                if i >= superset || out.last().is_some_and(|l| *l >= i) {
                    bail!("corrupt data: column index {i}");
                }
                out.push(i);
            }
        } else {
            let (mut i, mut skipped) = (0usize, 0usize);
            loop {
                let next_missing = if skipped < delta { idx(&mut self.r)? } else { superset };
                if next_missing < i {
                    bail!("corrupt data: column indexes out of order");
                }
                while i < next_missing {
                    out.push(i);
                    i += 1;
                }
                if i == superset {
                    break;
                }
                i += 1;
                skipped += 1;
            }
        }
        Ok(out)
    }

    fn cell(&mut self, row: &LivenessInfo, fixed: Option<usize>, complex: bool) -> Result<Cell> {
        let flags = self.r.u8()?;
        let deleted = flags & IS_DELETED != 0;
        let expiring = flags & IS_EXPIRING != 0;
        let use_row_ttl = flags & USE_ROW_TTL != 0;
        let timestamp = if flags & USE_ROW_TIMESTAMP != 0 { row.timestamp } else { self.timestamp()? };
        let ldt = if use_row_ttl {
            row.local_expiration_time
        } else if deleted || expiring {
            self.local_deletion_time()?
        } else {
            NO_DELETION_TIME
        };
        let ttl = if use_row_ttl {
            row.ttl
        } else if expiring {
            self.ttl()?
        } else {
            NO_TTL
        };
        if ttl < 0 {
            bail!("corrupt data: TTL {ttl}");
        }
        let path = if complex { Some(self.r.vint_bytes()?) } else { None };
        let value = if flags & HAS_EMPTY_VALUE == 0 { self.value(fixed)? } else { Bytes::new() };
        let ldt = self.decode_ldt(ldt, ttl);
        let mut cell = if deleted {
            Cell::tombstone(timestamp, ldt)
        } else if ttl != NO_TTL {
            Cell { timestamp, ttl, flags: 0, local_deletion_time: ldt, value, path: None }
        } else {
            Cell::live(timestamp, value)
        };
        cell.path = path;
        Ok(cell)
    }

    fn marker(&mut self) -> Result<Marker> {
        let kind = self.r.u8()?;
        let size = self.r.be16()? as usize;
        let values = if size == 0 { Vec::new() } else { self.clustering_values(size)? };
        let prefix = Clustering::new(values.iter().map_while(|v| v.as_deref()));
        let body = self.sizes()?;
        let start = self.r.remaining();
        let marker = match kind {
            // EXCL_END_BOUND, INCL_END_BOUND
            0 | 6 => Marker::close(Bound { prefix, after: kind == 6 }, self.delta_deletion()?),
            // INCL_START_BOUND, EXCL_START_BOUND
            1 | 7 => Marker::open(Bound { prefix, after: kind == 7 }, self.delta_deletion()?),
            // EXCL_END_INCL_START_BOUNDARY, INCL_END_EXCL_START_BOUNDARY
            2 | 5 => {
                let close = self.delta_deletion()?;
                let open = self.delta_deletion()?;
                Marker { bound: Bound { prefix, after: kind == 5 }, close: Some(close), open: Some(open) }
            }
            k => bail!("corrupt data: range tombstone bound kind {k}"),
        };
        let used = start - self.r.remaining();
        if used > body {
            bail!("corrupt data: a marker overruns its size");
        }
        self.r.skip(body - used)?;
        Ok(marker)
    }
}

/// `Cell.deletionTimeUnsignedIntegerToLong`.
fn unsigned_deletion_time(v: i32) -> i64 {
    if v == -1 { NO_DELETION_TIME } else { v as u32 as i64 }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::descriptor::Format;
    use crate::input::{SliceIn, put_uvint};
    use crate::stats::{EncodingStats, TIMESTAMP_EPOCH};
    use crate::types::SType;

    pub fn header() -> Header {
        let col = |n: &str, ty: SType| HeaderColumn { name: Bytes::copy_from_slice(n.as_bytes()), complex: ty.is_multicell(), ty };
        Header {
            stats: EncodingStats { min_timestamp: TIMESTAMP_EPOCH, min_local_deletion_time: 1_442_880_000, min_ttl: 0 },
            key_type: SType::Native("int"),
            clustering: vec![SType::Native("int"), SType::Native("text")],
            statics: vec![col("s", SType::Native("text"))],
            regulars: vec![col("a", SType::Native("int")), col("b", SType::Native("text")), col("l", SType::List { elem: Box::new(SType::Native("int")), frozen: false })],
        }
    }

    fn clustering(out: &mut Vec<u8>, c1: i32, c2: &str) {
        put_uvint(out, 0);
        out.extend_from_slice(&c1.to_be_bytes());
        put_uvint(out, c2.len() as u64);
        out.extend_from_slice(c2.as_bytes());
    }

    fn sized(out: &mut Vec<u8>, body: &[u8]) {
        put_uvint(out, body.len() as u64 + 1);
        put_uvint(out, 0);
        out.extend_from_slice(body);
    }

    /// A partition in the `nb` format: key 1, a static row, a row with a marker, a deleted
    /// row with a range tombstone around it, a row with a list.
    pub fn partition_nb() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&4u16.to_be_bytes());
        p.extend_from_slice(&1i32.to_be_bytes());
        p.extend_from_slice(&i32::MAX.to_be_bytes());
        p.extend_from_slice(&i64::MIN.to_be_bytes());
        // Static row: s = "st" at timestamp epoch + 5.
        p.push(EXTENSION_FLAG | HAS_ALL_COLUMNS);
        p.push(IS_STATIC);
        let mut b = Vec::new();
        b.push(0);
        put_uvint(&mut b, 5);
        put_uvint(&mut b, 2);
        b.extend_from_slice(b"st");
        sized(&mut p, &b);
        // Row (1, "x"): marker at +10 with TTL 100, a = 7 (row timestamp), b missing.
        p.push(HAS_TIMESTAMP | HAS_TTL);
        clustering(&mut p, 1, "x");
        let mut b = Vec::new();
        put_uvint(&mut b, 10);
        put_uvint(&mut b, 100);
        put_uvint(&mut b, 50);
        put_uvint(&mut b, 0b110); // b and l missing
        b.push(USE_ROW_TIMESTAMP | USE_ROW_TTL | IS_EXPIRING);
        b.extend_from_slice(&7i32.to_be_bytes());
        sized(&mut p, &b);
        // Marker: INCL_START (2) at +20.
        p.push(IS_MARKER);
        p.push(1);
        p.extend_from_slice(&1u16.to_be_bytes());
        put_uvint(&mut p, 0);
        p.extend_from_slice(&2i32.to_be_bytes());
        let mut b = Vec::new();
        put_uvint(&mut b, 20);
        put_uvint(&mut b, 30);
        sized(&mut p, &b);
        // Row (2, "y"): deleted at +15 (shadowable via the extended flag).
        p.push(HAS_DELETION | EXTENSION_FLAG);
        p.push(HAS_SHADOWABLE_DELETION);
        clustering(&mut p, 2, "y");
        let mut b = Vec::new();
        put_uvint(&mut b, 15);
        put_uvint(&mut b, 40);
        put_uvint(&mut b, 0b111);
        sized(&mut p, &b);
        // Marker: INCL_END (2).
        p.push(IS_MARKER);
        p.push(6);
        p.extend_from_slice(&1u16.to_be_bytes());
        put_uvint(&mut p, 0);
        p.extend_from_slice(&2i32.to_be_bytes());
        let mut b = Vec::new();
        put_uvint(&mut b, 20);
        put_uvint(&mut b, 30);
        sized(&mut p, &b);
        // Row (3, ""): list with a complex deletion and two cells, one a tombstone.
        p.push(HAS_COMPLEX_DELETION);
        put_uvint(&mut p, 1 << 2); // c2 empty
        p.extend_from_slice(&3i32.to_be_bytes());
        let mut b = Vec::new();
        put_uvint(&mut b, 0b011); // only l
        put_uvint(&mut b, 1);
        put_uvint(&mut b, 2);
        put_uvint(&mut b, 2);
        b.push(0);
        put_uvint(&mut b, 3);
        put_uvint(&mut b, 1);
        b.push(0xaa);
        put_uvint(&mut b, 4);
        b.extend_from_slice(&9i32.to_be_bytes());
        b.push(IS_DELETED | HAS_EMPTY_VALUE);
        put_uvint(&mut b, 4);
        put_uvint(&mut b, 60);
        put_uvint(&mut b, 1);
        b.push(0xbb);
        sized(&mut p, &b);
        p.push(END_OF_PARTITION);
        p
    }

    #[test]
    fn reads_rows_statics_markers_and_cells() {
        let mut data = partition_nb();
        data.extend_from_slice(&partition_nb());
        let v = Version::parse("nb", Format::Big).unwrap();
        let mut s = Scanner::new(SliceIn::new(Bytes::from(data)), Arc::new(header()), &v);
        let h = s.next_partition().unwrap().unwrap();
        assert_eq!(&h.key[..], &1i32.to_be_bytes());
        assert!(h.deletion.is_live());
        let st = h.static_row.unwrap();
        assert_eq!(st.cell(0).unwrap().value, "st");
        assert_eq!(st.cell(0).unwrap().timestamp, TIMESTAMP_EPOCH + 5);
        let Some(Item::Row(r)) = s.next_item().unwrap() else { panic!() };
        assert_eq!(r.clustering, Clustering::new([&1i32.to_be_bytes()[..], b"x"]));
        assert_eq!(r.liveness, LivenessInfo::expiring(TIMESTAMP_EPOCH + 10, 100, 1_442_880_050));
        let a = r.cell(0).unwrap();
        assert_eq!((a.timestamp, a.ttl, a.local_deletion_time, &a.value[..]), (TIMESTAMP_EPOCH + 10, 100, 1_442_880_050, &7i32.to_be_bytes()[..]));
        let Some(Item::Marker(m)) = s.next_item().unwrap() else { panic!() };
        assert_eq!(m.open, Some(DeletionTime::new(TIMESTAMP_EPOCH + 20, 1_442_880_030)));
        assert!(!m.bound.after);
        let Some(Item::Row(r)) = s.next_item().unwrap() else { panic!() };
        assert!(r.deletion.shadowable);
        assert_eq!(r.deletion.time, DeletionTime::new(TIMESTAMP_EPOCH + 15, 1_442_880_040));
        let Some(Item::Marker(m)) = s.next_item().unwrap() else { panic!() };
        assert!(m.close.is_some() && m.bound.after);
        let Some(Item::Row(r)) = s.next_item().unwrap() else { panic!() };
        assert_eq!(r.clustering.get(1).unwrap(), "");
        let ColumnData::Complex { column, deletion, cells } = &r.columns[0] else { panic!() };
        assert_eq!((*column, deletion.marked_at), (2, TIMESTAMP_EPOCH + 1));
        assert_eq!(cells[0].path.as_deref(), Some(&[0xaa][..]));
        assert!(cells[1].is_tombstone());
        assert_eq!(cells[1].local_deletion_time, 1_442_880_060);
        assert!(s.next_item().unwrap().is_none());
        // The second partition, skipped whole.
        assert!(s.next_partition().unwrap().is_some());
        assert!(s.next_partition().unwrap().is_none());
    }

    #[test]
    fn deletion_time_encodings() {
        let mut uint = vec![0x80];
        uint.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 5, 0xff, 0xff, 0xff, 0xf0, 0x01]);
        let oa = Version::parse("oa", Format::Big).unwrap();
        let mut s = Scanner::new(SliceIn::new(Bytes::from(uint[1..].to_vec())), Arc::new(header()), &oa);
        let d = s.partition_deletion().unwrap();
        assert_eq!(d, DeletionTime::new(5, 0xffff_fff0), "after 2038");
        let mut s = Scanner::new(SliceIn::new(Bytes::from(uint)), Arc::new(header()), &oa);
        assert!(s.partition_deletion().unwrap().is_live());
        let mut s = Scanner::new(SliceIn::new(Bytes::from_static(&[0x81])), Arc::new(header()), &oa);
        assert!(s.partition_deletion().is_err());
        let me = Version::parse("me", Format::Big).unwrap();
        let s = Scanner::new(SliceIn::new(Bytes::new()), Arc::new(header()), &me);
        assert_eq!(s.decode_ldt(-5, 10), INVALID_DELETION_TIME);
        let s = Scanner::new(SliceIn::new(Bytes::new()), Arc::new(header()), &oa);
        assert_eq!(s.decode_ldt(-16, 10), 0xffff_fff0);
        assert_eq!(s.decode_ldt(5, 10), INVALID_DELETION_TIME);
        assert_eq!(s.decode_ldt(5, i32::MAX), 5);
    }

    #[test]
    fn large_column_subsets() {
        // 70 columns, 3 present (indexes written) and 67 present (missing ones written).
        let col = HeaderColumn { name: Bytes::from_static(b"c"), ty: SType::Native("int"), complex: false };
        let mut h = header();
        h.regulars = vec![col; 70];
        let v = Version::parse("nb", Format::Big).unwrap();
        let mut b = Vec::new();
        put_uvint(&mut b, 67);
        for i in [1u64, 5, 69] {
            put_uvint(&mut b, i);
        }
        put_uvint(&mut b, 3);
        for i in [0u64, 10, 20] {
            put_uvint(&mut b, i);
        }
        let mut s = Scanner::new(SliceIn::new(Bytes::from(b)), Arc::new(h), &v);
        assert_eq!(s.subset(70).unwrap(), vec![1, 5, 69]);
        let rest = s.subset(70).unwrap();
        assert_eq!(rest.len(), 67);
        assert!(!rest.contains(&10) && rest.contains(&11));
    }

    #[test]
    fn corrupt_partitions_are_errors() {
        let good = partition_nb();
        let v = Version::parse("nb", Format::Big).unwrap();
        let scan_all = |data: Vec<u8>| -> Result<usize> {
            let mut s = Scanner::new(SliceIn::new(Bytes::from(data)), Arc::new(header()), &v);
            let mut n = 0;
            while s.next_partition()?.is_some() {
                while s.next_item()?.is_some() {
                    n += 1;
                }
            }
            Ok(n)
        };
        assert_eq!(scan_all(good.clone()).unwrap(), 5);
        for cut in 1..good.len() {
            assert!(scan_all(good[..cut].to_vec()).is_err(), "truncated at {cut}");
        }
        let mut bad = good.clone();
        bad[20] = 0xff; // the static row's size
        assert!(scan_all(bad).is_err());
    }
}
