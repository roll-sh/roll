//! `Statistics.db`: the metadata components (`org.apache.cassandra.io.sstable.metadata.
//! MetadataSerializer`), of which import needs the serialization header
//! (`SerializationHeader.Component`: key, clustering and column types, and the
//! `EncodingStats` bases of delta-encoded timestamps, TTLs and deletion times), the partitioner
//! (`ValidationMetadata`) and the partition count (`StatsMetadata`'s partition size histogram).

use crate::descriptor::Version;
use crate::input::{In, SliceIn};
use crate::types::SType;
use anyhow::{Context, Result, bail};
use bytes::Bytes;

/// `EncodingStats.TIMESTAMP_EPOCH`: 2015-09-22T00:00:00Z in microseconds.
pub const TIMESTAMP_EPOCH: i64 = 1_442_880_000_000_000;
/// `EncodingStats.DELETION_TIME_EPOCH`, in seconds.
pub const DELETION_TIME_EPOCH: i32 = 1_442_880_000;

/// `EncodingStats`: the bases of delta encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodingStats {
    pub min_timestamp: i64,
    pub min_local_deletion_time: i64,
    pub min_ttl: i32,
}

/// A static or regular column of the serialization header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderColumn {
    pub name: Bytes,
    pub ty: SType,
    /// Multi-cell (non-frozen collection or UDT).
    pub complex: bool,
}

impl HeaderColumn {
    pub fn name_str(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }
}

/// `SerializationHeader` as rows are read with it. Columns are in the order rows serialise
/// them (`Columns`: simple before complex, each by name bytes), not the file's map order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub stats: EncodingStats,
    pub key_type: SType,
    pub clustering: Vec<SType>,
    pub statics: Vec<HeaderColumn>,
    pub regulars: Vec<HeaderColumn>,
}

impl Header {
    pub fn columns(&self, is_static: bool) -> &[HeaderColumn] {
        if is_static { &self.statics } else { &self.regulars }
    }
}

#[derive(Clone, Debug)]
pub struct Statistics {
    pub header: Header,
    pub partitioner: Option<String>,
    /// From the partition size histogram (0 when absent).
    pub partitions: u64,
}

const VALIDATION: u32 = 0;
const STATS: u32 = 2;
const HEADER: u32 = 3;

fn check_crc(crc: &crc32fast::Hasher, r: &mut SliceIn, what: &str) -> Result<()> {
    let want = r.be32()?;
    if crc.clone().finalize() != want {
        bail!("corrupt Statistics.db: {what} checksum mismatch");
    }
    Ok(())
}

impl Statistics {
    pub fn parse(raw: Bytes, version: &Version) -> Result<Statistics> {
        let checksummed = version.metadata_checksum();
        let total = raw.len();
        let mut r = SliceIn::new(raw.clone());
        let mut crc = crc32fast::Hasher::new();
        let count = r.be32()?;
        crc.update(&count.to_be_bytes());
        if checksummed {
            check_crc(&crc, &mut r, "table of contents")?;
        }
        let count = r.count(count as u64, 8)?;
        let mut toc = Vec::with_capacity(count);
        for _ in 0..count {
            let (ty, off) = (r.be32()?, r.be32()?);
            crc.update(&ty.to_be_bytes());
            crc.update(&off.to_be_bytes());
            toc.push((ty, off as usize));
        }
        if checksummed {
            check_crc(&crc, &mut r, "table of contents")?;
        }
        let mut header = None;
        let mut partitioner = None;
        let mut partitions = 0;
        for (i, (ty, off)) in toc.iter().enumerate() {
            let end = toc.get(i + 1).map_or(total, |n| n.1);
            if *off > end || end > total {
                bail!("corrupt Statistics.db: component {ty} at {off}..{end} of {total}");
            }
            let mut body = raw.slice(*off..end);
            if checksummed {
                if body.len() < 4 {
                    bail!("corrupt Statistics.db: component {ty} too short");
                }
                let (data, sum) = body.split_at(body.len() - 4);
                if crc32fast::hash(data) != u32::from_be_bytes(sum.try_into().expect("4")) {
                    bail!("corrupt Statistics.db: component {ty} checksum mismatch");
                }
                body = body.slice(..body.len() - 4);
            }
            let mut c = SliceIn::new(body);
            match *ty {
                VALIDATION => partitioner = Some(c.utf()?),
                STATS => partitions = partition_count(&mut c).unwrap_or(0),
                HEADER => header = Some(parse_header(&mut c).context("serialization header")?),
                _ => {}
            }
        }
        let header = header.ok_or_else(|| anyhow::anyhow!("Statistics.db has no serialization header"))?;
        Ok(Statistics { header, partitioner, partitions })
    }
}

/// The total count of `StatsMetadata.estimatedPartitionSize` (an `EstimatedHistogram`).
fn partition_count(r: &mut SliceIn) -> Result<u64> {
    let n = r.be32()?;
    let n = r.count(n as u64, 16)?;
    let mut total = 0u64;
    for _ in 0..n {
        r.be64()?;
        total = total.saturating_add(r.be64()?);
    }
    Ok(total)
}

fn type_string(r: &mut SliceIn) -> Result<SType> {
    let b = r.vint_bytes()?;
    SType::parse(std::str::from_utf8(&b).context("type name")?)
}

fn parse_header(r: &mut SliceIn) -> Result<Header> {
    let min_timestamp = (r.uvint()? as i64).wrapping_add(TIMESTAMP_EPOCH);
    // Cassandra writes 32-bit deltas (`writeUnsignedVInt32`); ScyllaDB writes "none"
    // (`int64` max, relative to the epoch) when a memtable had no deletion times.
    let ldt = r.uvint()? as i64;
    let min_local_deletion_time = match i32::try_from(ldt) {
        Ok(d) => d.wrapping_add(DELETION_TIME_EPOCH) as i64,
        Err(_) => ldt.wrapping_add(DELETION_TIME_EPOCH as i64),
    };
    let min_ttl = r.uvint()? as i64 as i32;
    let key_type = type_string(r)?;
    let n = r.uvint()?;
    let n = r.count(n, 1)?;
    let mut clustering = Vec::with_capacity(n);
    for _ in 0..n {
        clustering.push(type_string(r)?);
    }
    let mut columns = || -> Result<Vec<HeaderColumn>> {
        let n = r.uvint()?;
        let n = r.count(n, 2)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let name = r.vint_bytes()?;
            let ty = type_string(r)?;
            let complex = ty.is_multicell();
            v.push(HeaderColumn { name, ty, complex });
        }
        // `ColumnMetadata.compareTo` within one kind: simple first, then by name bytes.
        v.sort_by(|a, b| (a.complex, &a.name[..]).cmp(&(b.complex, &b.name[..])));
        Ok(v)
    };
    let statics = columns()?;
    let regulars = columns()?;
    Ok(Header { stats: EncodingStats { min_timestamp, min_local_deletion_time, min_ttl }, key_type, clustering, statics, regulars })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::descriptor::Format;
    use crate::input::put_uvint;

    pub fn put_str(out: &mut Vec<u8>, s: &str) {
        put_uvint(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }

    /// A Statistics.db with a validation and a header component (checksummed or not).
    pub fn build(checksummed: bool, header: &[u8]) -> Vec<u8> {
        let mut validation = Vec::new();
        let p = b"org.apache.cassandra.dht.Murmur3Partitioner";
        validation.extend_from_slice(&(p.len() as u16).to_be_bytes());
        validation.extend_from_slice(p);
        validation.extend_from_slice(&0.01f64.to_be_bytes());
        let comps: Vec<(u32, Vec<u8>)> = vec![(VALIDATION, validation), (HEADER, header.to_vec())];
        let mut out = Vec::new();
        let mut crc = crc32fast::Hasher::new();
        out.extend_from_slice(&(comps.len() as u32).to_be_bytes());
        crc.update(&(comps.len() as u32).to_be_bytes());
        if checksummed {
            out.extend_from_slice(&crc.clone().finalize().to_be_bytes());
        }
        let toc_len = 8 * comps.len() + if checksummed { 4 } else { 0 };
        let mut off = out.len() + toc_len;
        for (t, body) in &comps {
            out.extend_from_slice(&t.to_be_bytes());
            out.extend_from_slice(&(off as u32).to_be_bytes());
            crc.update(&t.to_be_bytes());
            crc.update(&(off as u32).to_be_bytes());
            off += body.len() + if checksummed { 4 } else { 0 };
        }
        if checksummed {
            out.extend_from_slice(&crc.finalize().to_be_bytes());
        }
        for (_, body) in &comps {
            out.extend_from_slice(body);
            if checksummed {
                out.extend_from_slice(&crc32fast::hash(body).to_be_bytes());
            }
        }
        out
    }

    pub fn header_bytes() -> Vec<u8> {
        let m = "org.apache.cassandra.db.marshal.";
        let mut h = Vec::new();
        put_uvint(&mut h, 1000); // min timestamp delta
        put_uvint(&mut h, 10); // min ldt delta
        put_uvint(&mut h, 0); // min ttl
        put_str(&mut h, &format!("{m}Int32Type"));
        put_uvint(&mut h, 1);
        put_str(&mut h, &format!("{m}UTF8Type"));
        put_uvint(&mut h, 1);
        put_str(&mut h, "s");
        put_str(&mut h, &format!("{m}UTF8Type"));
        put_uvint(&mut h, 3);
        put_str(&mut h, "z");
        put_str(&mut h, &format!("{m}Int32Type"));
        put_str(&mut h, "l");
        put_str(&mut h, &format!("{m}ListType({m}Int32Type)"));
        put_str(&mut h, "a");
        put_str(&mut h, &format!("{m}UTF8Type"));
        h
    }

    #[test]
    fn parses_header_in_row_column_order() {
        for (checksummed, v) in [(true, "nb"), (false, "me")] {
            let raw = build(checksummed, &header_bytes());
            let s = Statistics::parse(Bytes::from(raw), &Version::parse(v, Format::Big).unwrap()).unwrap();
            assert_eq!(s.partitioner.as_deref(), Some("org.apache.cassandra.dht.Murmur3Partitioner"));
            assert_eq!(s.header.stats, EncodingStats { min_timestamp: TIMESTAMP_EPOCH + 1000, min_local_deletion_time: DELETION_TIME_EPOCH as i64 + 10, min_ttl: 0 });
            let names: Vec<String> = s.header.regulars.iter().map(|c| c.name_str()).collect();
            assert_eq!(names, ["a", "z", "l"], "simple columns by name, then complex");
            assert!(s.header.regulars[2].complex);
            assert_eq!(s.header.statics.len(), 1);
        }
    }

    #[test]
    fn corruption_is_an_error() {
        let raw = build(true, &header_bytes());
        let nb = Version::parse("nb", Format::Big).unwrap();
        let mut bad = raw.clone();
        let n = bad.len();
        bad[n - 6] ^= 0x40;
        assert!(Statistics::parse(Bytes::from(bad), &nb).unwrap_err().to_string().contains("checksum"));
        assert!(Statistics::parse(Bytes::from(raw[..raw.len() - 9].to_vec()), &nb).is_err());
        let mut toc = raw.clone();
        toc[3] = 0xff; // component count
        assert!(Statistics::parse(Bytes::from(toc), &nb).is_err());
    }
}
