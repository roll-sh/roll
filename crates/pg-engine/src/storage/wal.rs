//! Write-ahead log: one record per commit, `[len][crc][ts | entries]`, fsynced before the
//! commit becomes visible. A new file starts with every memtable; files are deleted once their
//! commits are in SSTs.

use super::sst::{get_varint, put_varint};
use crate::error::{Result, internal};
use bytes::Bytes;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use tracing::warn;

pub fn wal_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:016x}.wal"))
}

pub struct WalWriter {
    file: std::sync::Arc<File>,
    pub seq: u64,
    pub max_ts: u64,
    pub bytes: u64,
    buf: Vec<u8>,
    /// The file is zero-filled up to here, ahead of the records (0: not at all).
    zeroed: u64,
    /// Zero-fill this much at a time (0: never, records extend the file).
    prealloc: u64,
}

/// How far ahead a synced WAL is zero-filled: an fdatasync of records written into
/// allocated, already-sized space has no file metadata to flush (Postgres zero-fills its WAL
/// segments for the same reason), so most commits' fsyncs write just their data.
pub const PREALLOC: u64 = 1 << 20;

pub type Entry = (Vec<u8>, Option<Bytes>);

impl WalWriter {
    /// A new WAL file, zero-filled `prealloc` bytes at a time ahead of its records.
    pub fn create(dir: &Path, seq: u64, prealloc: u64) -> Result<WalWriter> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(wal_path(dir, seq))?;
        let bytes = file.metadata()?.len();
        Ok(WalWriter {
            file: std::sync::Arc::new(file),
            seq,
            max_ts: 0,
            bytes,
            buf: Vec::with_capacity(4096),
            zeroed: bytes,
            prealloc,
        })
    }

    pub fn append(&mut self, ts: u64, entries: &[Entry]) -> Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&[0u8; 8]);
        self.buf.extend_from_slice(&ts.to_le_bytes());
        put_varint(&mut self.buf, entries.len() as u64);
        for (k, v) in entries {
            put_varint(&mut self.buf, k.len() as u64);
            self.buf.extend_from_slice(k);
            match v {
                Some(v) => {
                    put_varint(&mut self.buf, v.len() as u64 + 1);
                    self.buf.extend_from_slice(v);
                }
                None => put_varint(&mut self.buf, 0),
            }
        }
        let len = (self.buf.len() - 8) as u32;
        let crc = crc32fast::hash(&self.buf[8..]);
        self.buf[0..4].copy_from_slice(&len.to_le_bytes());
        self.buf[4..8].copy_from_slice(&crc.to_le_bytes());
        let end = self.bytes + self.buf.len() as u64;
        if self.prealloc > 0 && end > self.zeroed {
            // (A zero record length ends replay: the zeroes past the last record are no
            // records.)
            let to = end.max(self.zeroed + self.prealloc);
            let zeros = vec![0u8; (to - self.zeroed) as usize];
            self.file.write_all_at(&zeros, self.zeroed)?;
            self.zeroed = to;
        }
        self.file.write_all_at(&self.buf, self.bytes)?;
        self.bytes = end;
        self.max_ts = self.max_ts.max(ts);
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// The file, to fsync without holding the writer (group commit).
    pub fn handle(&self) -> std::sync::Arc<File> {
        self.file.clone()
    }
}

/// Every WAL file in `dir`, by sequence number.
pub fn list(dir: &Path) -> Result<Vec<u64>> {
    let mut seqs = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let name = e?.file_name();
        let name = name.to_string_lossy();
        if let Some(hex) = name.strip_suffix(".wal")
            && let Ok(seq) = u64::from_str_radix(hex, 16)
        {
            seqs.push(seq);
        }
    }
    seqs.sort_unstable();
    Ok(seqs)
}

/// Replay a WAL file. A torn record at the end (crash mid-write) ends the file cleanly.
pub fn replay(path: &Path, mut f: impl FnMut(u64, Vec<Entry>)) -> Result<u64> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    let mut pos = 0usize;
    let mut max_ts = 0;
    while pos + 8 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        // The zero-filled space after the last record (`PREALLOC`).
        if len == 0 {
            break;
        }
        if pos + 8 + len > data.len() {
            warn!(path = %path.display(), "torn WAL record at end of file; ignoring it");
            break;
        }
        let rec = &data[pos + 8..pos + 8 + len];
        if crc32fast::hash(rec) != crc {
            warn!(path = %path.display(), offset = pos, "WAL checksum mismatch; stopping replay here");
            break;
        }
        let ts = u64::from_le_bytes(rec[0..8].try_into().unwrap());
        let mut p = 8;
        let n = get_varint(rec, &mut p)? as usize;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let kl = get_varint(rec, &mut p)? as usize;
            let k = rec
                .get(p..p + kl)
                .ok_or_else(|| internal("bad WAL record"))?
                .to_vec();
            p += kl;
            let vl = get_varint(rec, &mut p)? as usize;
            let v = if vl == 0 {
                None
            } else {
                let v = Bytes::copy_from_slice(
                    rec.get(p..p + vl - 1)
                        .ok_or_else(|| internal("bad WAL record"))?,
                );
                p += vl - 1;
                Some(v)
            };
            entries.push((k, v));
        }
        max_ts = max_ts.max(ts);
        f(ts, entries);
        pos += 8 + len;
    }
    Ok(max_ts)
}
