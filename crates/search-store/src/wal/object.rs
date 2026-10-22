//! The shipped form of a WAL file: records regrouped per shard, with a directory.
//!
//! A local WAL file interleaves every shard of every cluster on the node in log order. Replaying
//! one shard from it would mean decompressing everything, and after a node failure every one of
//! its shards does that, on whichever nodes they land. So when a sealed file ships, its records
//! are regrouped into one zstd block per shard (keeping each shard's log order), followed by a
//! directory naming each block's shard, seq_no range and time range. Replay decompresses only its
//! own block, and skips a file altogether when its block is absent or already covered by the
//! checkpoint.
//!
//! ```text
//! header (32 bytes): magic "RSWALZ\0\x01" | u64 file seq | u64 first lsn | u64 created ms
//! blocks: one zstd frame per shard, holding that shard's records (the local record format)
//! directory: zstd frame of
//!   u32 entries, then per entry: u16 cluster len | cluster | u16 uuid len | uuid | u32 shard |
//!   u64 block offset | u32 block len | u32 records | i64 min seq_no | i64 max seq_no |
//!   u64 first ms | u64 last ms
//! trailer (24 bytes): u64 directory offset | u32 directory len | u32 crc32c(directory) |
//!   magic "RSWALDIR"
//! ```
//!
//! The directory sits at the end so a reader can start from a suffix read: one request for
//! the tail (the whole object when it is small), one for the directory if the tail missed it,
//! and one for the block it wants, instead of downloading a 64 MiB file shared by every tenant
//! of the node to replay one shard.
//!
//! Every offset and count read back is checked against the bytes actually present before use,
//! and decompression grows buffers with the data produced, never to a size the object claims.

use super::record::{self, FILE_HEADER, FileHeader, Records};
use std::io::{Read, Write};

pub const OBJECT_MAGIC: [u8; 8] = *b"RSWALZ\0\x01";
const TRAILER_MAGIC: [u8; 8] = *b"RSWALDIR";
pub const TRAILER: usize = 24;
/// Directory bytes after decompression: generous (an entry is ~70 bytes plus names).
const MAX_DIR: u64 = 256 << 20;
/// A block holds at most one file's worth of records.
const MAX_BLOCK: u64 = 4 << 30;

/// One shard's block in a WAL object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub cluster: String,
    pub index_uuid: String,
    pub shard: u32,
    pub offset: u64,
    pub len: u32,
    pub records: u32,
    pub min_seq_no: i64,
    pub max_seq_no: i64,
    pub first_ms: u64,
    pub last_ms: u64,
}

/// A parsed WAL object.
#[derive(Debug)]
pub struct WalObject {
    pub header: FileHeader,
    pub entries: Vec<Entry>,
}

/// Regroup a local WAL file (header plus intact records) into its shipped form.
pub fn encode(file: &[u8], level: i32) -> anyhow::Result<Vec<u8>> {
    let header = FileHeader::decode(file).ok_or_else(|| anyhow::anyhow!("not a WAL file"))?;
    let body = &file[FILE_HEADER..];
    // Group record byte ranges by shard, in order of first appearance (deterministic output).
    let mut index: hashbrown::HashMap<(&str, &str, u32), usize> = hashbrown::HashMap::new();
    let mut groups: Vec<(Entry, Vec<(usize, usize)>)> = Vec::new();
    let mut it = Records::new(body);
    let mut start = 0;
    while let Some(r) = it.next() {
        let end = it.valid_len();
        let i = *index.entry((r.cluster, r.index_uuid, r.shard)).or_insert_with(|| {
            groups.push((
                Entry {
                    cluster: r.cluster.to_string(),
                    index_uuid: r.index_uuid.to_string(),
                    shard: r.shard,
                    offset: 0,
                    len: 0,
                    records: 0,
                    min_seq_no: r.seq_no,
                    max_seq_no: r.seq_no,
                    first_ms: r.at_ms,
                    last_ms: r.at_ms,
                },
                Vec::new(),
            ));
            groups.len() - 1
        });
        let (e, ranges) = &mut groups[i];
        e.records += 1;
        e.min_seq_no = e.min_seq_no.min(r.seq_no);
        e.max_seq_no = e.max_seq_no.max(r.seq_no);
        e.last_ms = e.last_ms.max(r.at_ms);
        match ranges.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => ranges.push((start, end)),
        }
        start = end;
    }
    anyhow::ensure!(start == body.len(), "WAL file {} is damaged at byte {}", header.seq, FILE_HEADER + start);

    let mut out = Vec::with_capacity(file.len() / 3 + 1024);
    let mut h = header.encode();
    h[..8].copy_from_slice(&OBJECT_MAGIC);
    out.extend_from_slice(&h);
    for (e, ranges) in &mut groups {
        e.offset = out.len() as u64;
        let mut enc = zstd::stream::Encoder::new(&mut out, level)?;
        enc.include_checksum(true)?;
        for &(a, b) in ranges.iter() {
            enc.write_all(&body[a..b])?;
        }
        enc.finish()?;
        e.len = u32::try_from(out.len() as u64 - e.offset).map_err(|_| anyhow::anyhow!("WAL block over 4 GiB"))?;
    }
    let mut dir = Vec::with_capacity(groups.len() * 80 + 4);
    dir.extend_from_slice(&(groups.len() as u32).to_le_bytes());
    for (e, _) in &groups {
        dir.extend_from_slice(&(e.cluster.len() as u16).to_le_bytes());
        dir.extend_from_slice(e.cluster.as_bytes());
        dir.extend_from_slice(&(e.index_uuid.len() as u16).to_le_bytes());
        dir.extend_from_slice(e.index_uuid.as_bytes());
        dir.extend_from_slice(&e.shard.to_le_bytes());
        dir.extend_from_slice(&e.offset.to_le_bytes());
        dir.extend_from_slice(&e.len.to_le_bytes());
        dir.extend_from_slice(&e.records.to_le_bytes());
        dir.extend_from_slice(&e.min_seq_no.to_le_bytes());
        dir.extend_from_slice(&e.max_seq_no.to_le_bytes());
        dir.extend_from_slice(&e.first_ms.to_le_bytes());
        dir.extend_from_slice(&e.last_ms.to_le_bytes());
    }
    let dir = zstd::bulk::compress(&dir, level)?;
    let dir_offset = out.len() as u64;
    out.extend_from_slice(&dir);
    out.extend_from_slice(&dir_offset.to_le_bytes());
    out.extend_from_slice(&(dir.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c::crc32c(&dir).to_le_bytes());
    out.extend_from_slice(&TRAILER_MAGIC);
    Ok(out)
}

fn inflate(data: &[u8], limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let n = zstd::stream::read::Decoder::with_buffer(data)?.take(limit + 1).read_to_end(&mut out)?;
    anyhow::ensure!((n as u64) <= limit, "WAL object block decompresses past {limit} bytes");
    Ok(out)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(self.0.len() >= n, "WAL object directory is truncated");
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u16(&mut self) -> anyhow::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> anyhow::Result<String> {
        let n = self.u16()? as usize;
        Ok(std::str::from_utf8(self.take(n)?)?.to_string())
    }
}

/// The fixed-size end of a WAL object: where its directory is and the directory's checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    pub dir_offset: u64,
    pub dir_len: u32,
    pub crc: u32,
}

impl Trailer {
    /// Decode the last [`TRAILER`] bytes of an object of `size` bytes and check that the
    /// directory they point at lies between the header and the trailer.
    pub fn decode(t: &[u8], size: u64) -> anyhow::Result<Trailer> {
        anyhow::ensure!(t.len() == TRAILER && size >= (FILE_HEADER + TRAILER) as u64, "not a WAL object");
        anyhow::ensure!(t[16..24] == TRAILER_MAGIC, "WAL object trailer is damaged");
        let tr = Trailer {
            dir_offset: u64::from_le_bytes(t[0..8].try_into().unwrap()),
            dir_len: u32::from_le_bytes(t[8..12].try_into().unwrap()),
            crc: u32::from_le_bytes(t[12..16].try_into().unwrap()),
        };
        anyhow::ensure!(
            tr.dir_offset >= FILE_HEADER as u64 && tr.dir_offset.checked_add(tr.dir_len as u64) == Some(size - TRAILER as u64),
            "WAL object directory is out of bounds"
        );
        Ok(tr)
    }

    /// Parse the directory bytes (`dir_offset..dir_offset + dir_len` of the object).
    pub fn directory(&self, dir: &[u8]) -> anyhow::Result<Vec<Entry>> {
        anyhow::ensure!(dir.len() == self.dir_len as usize, "WAL object directory is truncated");
        anyhow::ensure!(crc32c::crc32c(dir) == self.crc, "WAL object directory checksum mismatch");
        let raw = inflate(dir, MAX_DIR)?;
        let mut c = Cursor(&raw);
        let count = c.u32()? as usize;
        // Each entry takes at least 56 bytes: bound the count by what is there.
        anyhow::ensure!(count <= raw.len() / 56 + 1, "WAL object directory claims {count} entries");
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let e = Entry {
                cluster: c.str()?,
                index_uuid: c.str()?,
                shard: c.u32()?,
                offset: c.u64()?,
                len: c.u32()?,
                records: c.u32()?,
                min_seq_no: c.u64()? as i64,
                max_seq_no: c.u64()? as i64,
                first_ms: c.u64()?,
                last_ms: c.u64()?,
            };
            anyhow::ensure!(
                e.offset >= FILE_HEADER as u64 && e.offset.checked_add(e.len as u64).is_some_and(|end| end <= self.dir_offset),
                "WAL object block out of bounds"
            );
            entries.push(e);
        }
        Ok(entries)
    }
}

impl WalObject {
    pub fn parse(data: &[u8]) -> anyhow::Result<WalObject> {
        anyhow::ensure!(data.len() >= FILE_HEADER + TRAILER && data[..8] == OBJECT_MAGIC, "not a WAL object");
        let mut h = [0u8; FILE_HEADER];
        h.copy_from_slice(&data[..FILE_HEADER]);
        h[..8].copy_from_slice(&record::MAGIC);
        let header = FileHeader::decode(&h).expect("magic patched");
        let size = data.len() as u64;
        let t = Trailer::decode(&data[data.len() - TRAILER..], size)?;
        let entries = t.directory(&data[t.dir_offset as usize..(size as usize - TRAILER)])?;
        Ok(WalObject { header, entries })
    }

    /// Inflate one block fetched on its own (a ranged read of `e.offset..e.offset + e.len`).
    pub fn inflate_block(block: &[u8], e: &Entry) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(block.len() == e.len as usize, "WAL object block is truncated");
        inflate(block, MAX_BLOCK)
    }

    /// The decompressed records of one entry's block.
    pub fn block(data: &[u8], e: &Entry) -> anyhow::Result<Vec<u8>> {
        inflate(&data[e.offset as usize..(e.offset + e.len as u64) as usize], MAX_BLOCK)
    }
}

/// Whether bytes are a regrouped object (as opposed to a whole-file zstd frame).
pub fn is_object(data: &[u8]) -> bool {
    data.len() >= 8 && data[..8] == OBJECT_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> Vec<u8> {
        let mut f = FileHeader { seq: 4, first_lsn: 10, created_ms: 1 }.encode().to_vec();
        for i in 0..300i64 {
            let (c, u, s) = match i % 3 {
                0 => ("c1", "u1", 0),
                1 => ("c2", "u1", 0),
                _ => ("c1", "u1", 1),
            };
            record::encode(&mut f, 100 + i as u64, i, c, u, s, format!("op{i}").as_bytes());
        }
        f
    }

    #[test]
    fn regroups_per_shard_keeping_order() {
        let f = file();
        let obj = encode(&f, 3).unwrap();
        assert!(is_object(&obj));
        let parsed = WalObject::parse(&obj).unwrap();
        assert_eq!(parsed.header, FileHeader { seq: 4, first_lsn: 10, created_ms: 1 });
        assert_eq!(parsed.entries.len(), 3);
        let e = &parsed.entries[1];
        assert_eq!((e.cluster.as_str(), e.index_uuid.as_str(), e.shard, e.records), ("c2", "u1", 0, 100));
        assert_eq!((e.min_seq_no, e.max_seq_no, e.first_ms, e.last_ms), (1, 298, 101, 398));
        let block = WalObject::block(&obj, e).unwrap();
        let seqs: Vec<i64> = Records::new(&block).map(|r| r.seq_no).collect();
        assert_eq!(seqs, (0..100).map(|i| i * 3 + 1).collect::<Vec<_>>());
        assert!(Records::new(&block).all(|r| r.payload == format!("op{}", r.seq_no).as_bytes()));
    }

    #[test]
    fn empty_and_damaged() {
        let empty = FileHeader { seq: 1, first_lsn: 1, created_ms: 0 }.encode();
        let obj = encode(&empty, 1).unwrap();
        assert!(WalObject::parse(&obj).unwrap().entries.is_empty());
        let mut torn = file();
        torn.truncate(torn.len() - 3);
        assert!(encode(&torn, 1).is_err(), "only intact files ship");
        let good = encode(&file(), 1).unwrap();
        for cut in [0, 10, 40, good.len() - 30, good.len() - 1] {
            assert!(WalObject::parse(&good[..cut]).is_err(), "cut {cut}");
        }
        for i in [good.len() - 20, good.len() - 10, good.len() - 30] {
            let mut bad = good.clone();
            bad[i] ^= 0xff;
            assert!(WalObject::parse(&bad).is_err(), "flip {i}");
        }
        // The directory read from the tail alone, as a ranged reader does.
        let size = good.len() as u64;
        let t = Trailer::decode(&good[good.len() - TRAILER..], size).unwrap();
        let entries = t.directory(&good[t.dir_offset as usize..good.len() - TRAILER]).unwrap();
        assert_eq!(entries, WalObject::parse(&good).unwrap().entries);
        let e = &entries[2];
        let block = WalObject::inflate_block(&good[e.offset as usize..(e.offset + e.len as u64) as usize], e).unwrap();
        assert_eq!(block, WalObject::block(&good, e).unwrap());
        assert!(WalObject::inflate_block(&good[e.offset as usize..(e.offset + e.len as u64 - 1) as usize], e).is_err());
        assert!(Trailer::decode(&good[good.len() - TRAILER..], size + 1).is_err(), "size must match the trailer");
        assert!(t.directory(&good[t.dir_offset as usize + 1..good.len() - TRAILER]).is_err());
        // A block that doesn't decompress is an error, not garbage records.
        let parsed = WalObject::parse(&good).unwrap();
        let mut bad = good.clone();
        let e = &parsed.entries[0];
        bad[e.offset as usize + e.len as usize / 2] ^= 0x55;
        assert!(WalObject::block(&bad, e).is_err());
    }
}
