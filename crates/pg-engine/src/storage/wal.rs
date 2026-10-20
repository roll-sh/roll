//! Write-ahead log: one record per commit, `[len][crc][ts | entries]`, fsynced before the
//! commit becomes visible. A new file starts with every memtable; files are deleted once their
//! commits are in SSTs.

use super::sst::{get_varint, put_varint};
use crate::error::{Result, internal};
use bytes::Bytes;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::warn;

pub fn wal_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:016x}.wal"))
}

pub struct WalWriter {
    file: File,
    pub seq: u64,
    pub max_ts: u64,
    pub bytes: u64,
    buf: Vec<u8>,
}

pub type Entry = (Vec<u8>, Option<Bytes>);

impl WalWriter {
    pub fn create(dir: &Path, seq: u64) -> Result<WalWriter> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(wal_path(dir, seq))?;
        Ok(WalWriter {
            file,
            seq,
            max_ts: 0,
            bytes: 0,
            buf: Vec::with_capacity(4096),
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
        self.file.write_all(&self.buf)?;
        self.bytes += self.buf.len() as u64;
        self.max_ts = self.max_ts.max(ts);
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
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
