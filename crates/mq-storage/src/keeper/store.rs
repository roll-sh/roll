//! One log on a keeper's disk: segment files in `mq_core::log::FileLog`'s frame format
//! (`[len u32][crc32 u32][entry]`, named `{first:020}.seg`) plus a small `meta.json` with the
//! fencing epoch, truncation points and epoch history. Segments may start at any index (the
//! writer decides where they are sealed, and prefixes are dropped), and suffixes can be
//! truncated when a replica holds entries that were never committed.
//!
//! With a WAL (see `super`), appends reach the segments without fsync and are durable through
//! the shard's WAL. `wal_gen` in `meta.json` tells replay which WAL records still apply: it
//! starts random, and every operation that removes entries fsyncs the segment first and then
//! bumps it, so replay can never bring truncated entries back.

use crate::proto::{History, LogState, Reply, prev_epoch, prune_history};
use bytes::{BufMut, Bytes, BytesMut};
use mq_core::log::{crc32, encode_component};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Keepers start a new segment file on their own past this size, whatever the writer asks.
const HARD_SEGMENT_BYTES: u64 = 1 << 30;
/// Most a single read returns.
const READ_MAX_BYTES: usize = 16 << 20;
const READ_MAX_ENTRIES: usize = 65_536;

#[derive(Serialize, Deserialize, Default, Clone)]
struct Meta {
    key: String,
    epoch: u64,
    trim: u64,
    tiered: u64,
    /// `next` when the log holds no segments.
    floor: u64,
    history: History,
    /// WAL records for this log apply only while they carry this generation.
    #[serde(default)]
    wal_gen: u64,
}

struct Seg {
    first: u64,
    path: PathBuf,
    /// Byte offset of each entry.
    offsets: Vec<u32>,
    len: u64,
}

pub(crate) struct LogDir {
    dir: PathBuf,
    meta: Meta,
    segs: Vec<Seg>,
    active: Option<File>,
    next: u64,
    exists: bool,
    /// Written since the last fsync.
    pub dirty: bool,
    fsync: bool,
}

type Res<T> = Result<T, Reply>;

fn io<T>(r: std::io::Result<T>) -> Res<T> {
    r.map_err(|e| Reply::Error(format!("keeper i/o: {e}")))
}

/// Directory name for a log key (long keys are shortened and disambiguated by a hash).
pub(crate) fn dir_name(key: &str) -> String {
    let enc = encode_component(key);
    if enc.len() <= 200 {
        return enc;
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    let cut = enc.floor_char_boundary(150);
    format!("{}~{h:016x}", &enc[..cut])
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

fn scan(path: &Path, is_last: bool) -> std::io::Result<(Vec<u32>, u64)> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    let mut offsets = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let crc = u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
        if pos + 8 + len > data.len() || crc32(&data[pos + 8..pos + 8 + len]) != crc {
            break;
        }
        offsets.push(pos as u32);
        pos += 8 + len;
    }
    if pos < data.len() {
        if !is_last {
            return Err(std::io::Error::other(format!("corrupt segment {} at byte {pos}", path.display())));
        }
        // A torn write at the tail was never acknowledged.
        let f = OpenOptions::new().write(true).open(path)?;
        f.set_len(pos as u64)?;
        f.sync_all()?;
    }
    Ok((offsets, pos as u64))
}

impl LogDir {
    /// Load a log (or an empty, not yet created one) from `root/<dir_name(key)>`.
    pub fn open(root: &Path, key: &str, fsync: bool) -> std::io::Result<LogDir> {
        let dir = root.join(dir_name(key));
        let mut log = LogDir {
            dir: dir.clone(),
            meta: Meta { key: key.to_string(), ..Default::default() },
            segs: Vec::new(),
            active: None,
            next: 0,
            exists: false,
            dirty: false,
            fsync,
        };
        let meta_path = dir.join("meta.json");
        if !meta_path.exists() {
            return Ok(log);
        }
        log.exists = true;
        log.meta = serde_json::from_slice(&std::fs::read(&meta_path)?).map_err(std::io::Error::other)?;
        let mut names: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let stem = p.file_stem()?.to_str()?.to_string();
                (p.extension()? == "seg").then(|| stem.parse().ok().map(|n| (n, p)))?
            })
            .collect();
        names.sort();
        let count = names.len();
        for (i, (first, path)) in names.into_iter().enumerate() {
            // Segments must be contiguous; anything after a gap is unreachable garbage.
            if let Some(prev) = log.segs.last()
                && prev.first + prev.offsets.len() as u64 != first
            {
                std::fs::remove_file(&path).ok();
                continue;
            }
            let (offsets, len) = scan(&path, i + 1 == count)?;
            log.segs.push(Seg { first, path, offsets, len });
        }
        log.next = match log.segs.last() {
            Some(s) => s.first + s.offsets.len() as u64,
            None => log.meta.floor.max(log.meta.trim).max(log.meta.tiered),
        };
        let next = log.next;
        log.meta.history.retain(|(s, _)| *s < next);
        Ok(log)
    }

    fn hot_first(&self) -> u64 {
        self.segs.first().map(|s| s.first).unwrap_or(self.next).max(self.meta.trim).min(self.next)
    }

    pub fn state(&self) -> LogState {
        let segments = self.segs.iter().filter(|s| s.len > 0).map(|s| (s.first, s.len)).collect();
        LogState {
            epoch: self.meta.epoch,
            trim: self.meta.trim,
            tiered: self.meta.tiered,
            next: self.next,
            hot_first: self.hot_first(),
            history: self.meta.history.clone(),
            segments,
        }
    }

    fn save_meta(&mut self) -> Res<()> {
        if !self.exists {
            self.meta.wal_gen = mq_core::rand_u64();
            io(std::fs::create_dir_all(&self.dir))?;
            if let Some(parent) = self.dir.parent() {
                sync_dir(parent);
            }
            self.exists = true;
        }
        let tmp = self.dir.join("meta.json.tmp");
        let data = serde_json::to_vec(&self.meta).map_err(|e| Reply::Error(e.to_string()))?;
        let mut f = io(File::create(&tmp))?;
        io(f.write_all(&data))?;
        if self.fsync {
            io(f.sync_all())?;
        }
        io(std::fs::rename(&tmp, self.dir.join("meta.json")))?;
        if self.fsync {
            sync_dir(&self.dir);
        }
        Ok(())
    }

    fn check_epoch(&mut self, epoch: u64) -> Res<()> {
        if epoch < self.meta.epoch {
            return Err(Reply::Fenced { epoch: self.meta.epoch });
        }
        if epoch > self.meta.epoch {
            self.meta.epoch = epoch;
            self.save_meta()?;
        }
        Ok(())
    }

    pub fn fence(&mut self, epoch: u64, strict: bool) -> Reply {
        if strict && epoch <= self.meta.epoch {
            return Reply::Fenced { epoch: self.meta.epoch };
        }
        match self.check_epoch(epoch) {
            Ok(()) => Reply::State(self.state()),
            Err(r) => r,
        }
    }

    pub fn append(&mut self, epoch: u64, first: u64, prev: u64, entry_epoch: u64, seal: bool, entries: &[Bytes]) -> Reply {
        match self.try_append(epoch, first, prev, entry_epoch, seal, entries) {
            Ok(()) => Reply::Appended { next: self.next },
            Err(r) => r,
        }
    }

    fn try_append(&mut self, epoch: u64, first: u64, prev: u64, entry_epoch: u64, seal: bool, entries: &[Bytes]) -> Res<()> {
        self.check_epoch(epoch)?;
        if first != self.next || prev_epoch(&self.meta.history, first) != prev {
            return Err(Reply::Mismatch { next: self.next, last_epoch: prev_epoch(&self.meta.history, self.next) });
        }
        if entry_epoch > epoch {
            return Err(Reply::Error(format!("entry epoch {entry_epoch} is newer than the writer's {epoch}")));
        }
        if entries.is_empty() {
            return Ok(());
        }
        match self.meta.history.last().copied() {
            Some((_, e)) if e == entry_epoch => {}
            Some((_, e)) if e > entry_epoch => {
                return Err(Reply::Error(format!("entry epoch {entry_epoch} is older than the log's last epoch {e}")));
            }
            Some((s, _)) if s == first => {
                self.meta.history.pop();
                self.meta.history.push((first, entry_epoch));
                self.save_meta()?;
            }
            _ => {
                self.meta.history.push((first, entry_epoch));
                self.save_meta()?;
            }
        }
        if !self.exists {
            self.save_meta()?;
        }
        self.write(first, seal, entries)
    }

    /// Write entries at `first` (== `next`) to the segments, starting a new one if asked to.
    fn write(&mut self, first: u64, seal: bool, entries: &[Bytes]) -> Res<()> {
        let need_new = match self.segs.last() {
            None => true,
            Some(s) => (seal && s.len > 0) || s.len >= HARD_SEGMENT_BYTES,
        };
        if need_new {
            let path = self.dir.join(format!("{first:020}.seg"));
            io(OpenOptions::new().create(true).write(true).truncate(true).open(&path))?;
            if self.fsync {
                sync_dir(&self.dir);
            }
            // An empty trailing segment is replaced rather than left behind.
            if self.segs.last().is_some_and(|s| s.len == 0 && s.first != first) {
                let s = self.segs.pop().unwrap_or_else(|| unreachable!());
                std::fs::remove_file(&s.path).ok();
            }
            if self.segs.last().is_none_or(|s| s.first != first) {
                self.segs.push(Seg { first, path, offsets: Vec::new(), len: 0 });
            }
            // The closed segment's WAL-covered writes must not depend on a later checkpoint.
            self.sync_active()?;
            self.active = None;
        }
        let Some(seg) = self.segs.last_mut() else { return Err(Reply::Error("no active segment".into())) };
        if self.active.is_none() {
            self.active = Some(io(OpenOptions::new().write(true).open(&seg.path))?);
        }
        let mut buf = BytesMut::with_capacity(entries.iter().map(|e| 8 + e.len()).sum());
        let mut offsets = Vec::with_capacity(entries.len());
        let mut pos = seg.len;
        for e in entries {
            offsets.push(pos as u32);
            buf.put_u32(e.len() as u32);
            buf.put_u32(crc32(e));
            buf.put_slice(e);
            pos += 8 + e.len() as u64;
        }
        let f = self.active.as_ref().ok_or_else(|| Reply::Error("no active segment".into()))?;
        if let Err(e) = f.write_all_at(&buf, seg.len) {
            // Leave the file as it was: a later scan would drop the torn tail anyway.
            let _ = f.set_len(seg.len);
            return Err(Reply::Error(format!("keeper i/o: {e}")));
        }
        seg.offsets.extend_from_slice(&offsets);
        seg.len = pos;
        self.next += entries.len() as u64;
        self.dirty = true;
        Ok(())
    }

    fn sync_active(&mut self) -> Res<()> {
        if self.fsync
            && let Some(f) = &self.active
        {
            io(f.sync_data())?;
        }
        self.dirty = false;
        Ok(())
    }

    /// Before removing entries: make what's written durable and retire this log's WAL records.
    /// The new generation is persisted by the caller's `save_meta`.
    fn barrier(&mut self) -> Res<()> {
        self.sync_active()?;
        self.meta.wal_gen = self.meta.wal_gen.wrapping_add(1);
        Ok(())
    }

    pub fn wal_gen(&self) -> u64 {
        self.meta.wal_gen
    }

    /// Hand unsynced writes to a checkpoint: a second handle on the active segment to fsync.
    pub fn take_unsynced(&mut self) -> Option<File> {
        if !self.dirty || !self.fsync {
            return None;
        }
        let f = self.active.as_ref()?;
        match f.try_clone() {
            Ok(dup) => {
                self.dirty = false;
                Some(dup)
            }
            Err(_) => {
                let _ = self.sync();
                None
            }
        }
    }

    /// Re-apply a WAL append after a crash. Records from another generation, for a log that no
    /// longer exists, or already on disk are skipped.
    pub fn replay(&mut self, gen_: u64, first: u64, entries: &[Bytes]) -> Res<()> {
        if !self.exists || gen_ != self.meta.wal_gen {
            return Ok(());
        }
        if first > self.next {
            return Err(Reply::Error(format!("WAL record at {first} but the log ends at {}", self.next)));
        }
        let skip = (self.next - first) as usize;
        if skip < entries.len() {
            self.write(self.next, false, &entries[skip..])?;
        }
        Ok(())
    }

    /// fsync appended data (group commit: called once per batch of requests).
    pub fn sync(&mut self) -> std::io::Result<()> {
        if self.dirty {
            self.dirty = false;
            if self.fsync
                && let Some(f) = &self.active
            {
                f.sync_data()?;
            }
        }
        Ok(())
    }

    pub fn read(&self, from: u64, max: usize, max_bytes: usize) -> Reply {
        let from = from.max(self.meta.trim);
        if from >= self.next {
            return Reply::Entries { first: from, entries: Vec::new() };
        }
        let hot = self.hot_first();
        if from < hot {
            return Reply::NotHere { hot_first: hot };
        }
        let (max, max_bytes) = (max.clamp(1, READ_MAX_ENTRIES), max_bytes.min(READ_MAX_BYTES));
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut idx = from;
        let start_seg = self.segs.partition_point(|s| s.first <= from).saturating_sub(1);
        for seg in &self.segs[start_seg..] {
            let end = seg.first + seg.offsets.len() as u64;
            if idx < seg.first || idx >= end {
                continue;
            }
            let start = (idx - seg.first) as usize;
            let entry_end = |j: usize| seg.offsets.get(j + 1).map(|o| *o as u64).unwrap_or(seg.len);
            let mut stop = start;
            while stop < seg.offsets.len() && out.len() + (stop - start) < max {
                let len = (entry_end(stop) - seg.offsets[stop] as u64 - 8) as usize;
                if bytes > 0 && bytes + len > max_bytes {
                    break;
                }
                bytes += len;
                stop += 1;
            }
            if stop == start {
                break;
            }
            let lo = seg.offsets[start] as u64;
            let hi = entry_end(stop - 1);
            let mut data = vec![0u8; (hi - lo) as usize];
            let read = File::open(&seg.path).and_then(|f| f.read_exact_at(&mut data, lo));
            if let Err(e) = read {
                return Reply::Error(format!("keeper i/o: {e}"));
            }
            let data = Bytes::from(data);
            let mut pos = 0usize;
            for _ in start..stop {
                let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
                out.push(data.slice(pos + 8..pos + 8 + len));
                pos += 8 + len;
                idx += 1;
            }
            if out.len() >= max || stop < seg.offsets.len() {
                break;
            }
        }
        Reply::Entries { first: from, entries: out }
    }

    /// Delete segments that end at or before `upto`.
    fn drop_segments_before(&mut self, upto: u64) {
        while !self.segs.is_empty() {
            let end = self.segs.get(1).map(|n| n.first).unwrap_or(self.next);
            if end > upto {
                break;
            }
            let s = self.segs.remove(0);
            std::fs::remove_file(&s.path).ok();
            if self.segs.is_empty() {
                self.active = None;
                self.meta.floor = self.next;
            }
        }
    }

    pub fn truncate_prefix(&mut self, epoch: u64, before: u64) -> Reply {
        if let Err(r) = self.check_epoch(epoch) {
            return r;
        }
        let t = before.min(self.next);
        if t > self.meta.trim {
            if let Err(r) = self.barrier() {
                return r;
            }
            self.meta.trim = t;
            self.drop_segments_before(self.meta.trim.max(self.meta.tiered));
            prune_history(&mut self.meta.history, t);
            if let Err(r) = self.save_meta() {
                return r;
            }
        }
        Reply::State(self.state())
    }

    pub fn tiered(&mut self, epoch: u64, upto: u64) -> Reply {
        if let Err(r) = self.check_epoch(epoch) {
            return r;
        }
        let t = upto.min(self.next);
        if t > self.meta.tiered {
            if let Err(r) = self.barrier() {
                return r;
            }
            self.meta.tiered = t;
            self.drop_segments_before(self.meta.trim.max(t));
            if let Err(r) = self.save_meta() {
                return r;
            }
        }
        Reply::State(self.state())
    }

    pub fn truncate_suffix(&mut self, epoch: u64, from: u64) -> Reply {
        if let Err(r) = self.check_epoch(epoch) {
            return r;
        }
        if from >= self.next {
            return Reply::State(self.state());
        }
        if from < self.hot_first() {
            return Reply::Error(format!("cannot truncate at {from}: entries before {} are not on this keeper", self.hot_first()));
        }
        if let Err(r) = self.barrier() {
            return r;
        }
        self.active = None;
        while let Some(s) = self.segs.last() {
            if s.first < from {
                break;
            }
            let s = self.segs.pop().unwrap_or_else(|| unreachable!());
            std::fs::remove_file(&s.path).ok();
        }
        if let Some(s) = self.segs.last_mut() {
            let keep = (from - s.first) as usize;
            if keep < s.offsets.len() {
                let cut = s.offsets[keep] as u64;
                let res = OpenOptions::new().write(true).open(&s.path).and_then(|f| {
                    f.set_len(cut)?;
                    f.sync_all()
                });
                if let Err(e) = res {
                    return Reply::Error(format!("keeper i/o: {e}"));
                }
                s.offsets.truncate(keep);
                s.len = cut;
            }
        }
        self.next = from;
        if self.segs.is_empty() {
            self.meta.floor = from;
        }
        self.meta.history.retain(|(s, _)| *s < from);
        match self.save_meta() {
            Ok(()) => Reply::State(self.state()),
            Err(r) => r,
        }
    }

    pub fn reset(&mut self, epoch: u64, at: u64, prev: u64, trim: u64, tiered: u64) -> Reply {
        if let Err(r) = self.check_epoch(epoch) {
            return r;
        }
        if let Err(r) = self.barrier() {
            return r;
        }
        self.active = None;
        for s in self.segs.drain(..) {
            std::fs::remove_file(&s.path).ok();
        }
        self.next = at;
        self.meta.floor = at;
        self.meta.trim = trim.min(at);
        self.meta.tiered = tiered.min(at);
        self.meta.history = if at > 0 { vec![(0, prev)] } else { Vec::new() };
        match self.save_meta() {
            Ok(()) => Reply::State(self.state()),
            Err(r) => r,
        }
    }

    pub fn destroy(&mut self, epoch: u64) -> Reply {
        if let Err(r) = self.check_epoch(epoch) {
            return r;
        }
        self.active = None;
        self.segs.clear();
        if self.exists {
            std::fs::remove_dir_all(&self.dir).ok();
        }
        self.exists = false;
        let epoch = self.meta.epoch;
        self.meta = Meta { key: self.meta.key.clone(), epoch, ..Default::default() };
        self.next = 0;
        self.dirty = false;
        Reply::State(self.state())
    }
}

impl Drop for LogDir {
    fn drop(&mut self) {
        // Evicted logs may hold writes only the WAL covers.
        let _ = self.sync();
    }
}

/// Keys of every log under `root`.
pub(crate) fn list_logs(root: &Path) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return Ok(out) };
    for e in rd.flatten() {
        let meta = e.path().join("meta.json");
        if let Ok(data) = std::fs::read(&meta)
            && let Ok(m) = serde_json::from_slice::<Meta>(&data)
        {
            out.push(m.key);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(r: std::ops::Range<u32>) -> Vec<Bytes> {
        r.map(|i| Bytes::from(format!("e{i}"))).collect()
    }

    fn read_all(l: &LogDir, from: u64) -> Vec<String> {
        match l.read(from, 1000, 1 << 20) {
            Reply::Entries { entries, .. } => entries.iter().map(|e| String::from_utf8_lossy(e).into_owned()).collect(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn append_fence_truncate_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = LogDir::open(dir.path(), "q/%2F/a/1", true).unwrap();
        assert!(matches!(l.fence(1, true), Reply::State(_)));
        assert!(matches!(l.fence(1, true), Reply::Fenced { epoch: 1 }));
        assert_eq!(l.append(1, 0, 0, 1, false, &entries(0..10)), Reply::Appended { next: 10 });
        // Gaps and stale epochs are refused with the keeper's tail.
        assert_eq!(l.append(1, 12, 1, 1, false, &entries(0..1)), Reply::Mismatch { next: 10, last_epoch: 1 });
        assert!(matches!(l.fence(2, false), Reply::State(_)));
        assert_eq!(l.append(1, 10, 1, 1, false, &entries(10..11)), Reply::Fenced { epoch: 2 });
        // A wrong predecessor epoch is a mismatch even when the index lines up.
        assert!(matches!(l.append(2, 10, 7, 2, false, &entries(10..11)), Reply::Mismatch { .. }));
        assert_eq!(l.append(2, 10, 1, 2, true, &entries(10..20)), Reply::Appended { next: 20 });
        l.sync().unwrap();
        assert_eq!(read_all(&l, 8)[..3], ["e8", "e9", "e10"]);
        assert_eq!(l.state().history, vec![(0, 1), (10, 2)]);
        assert_eq!(l.state().segments.len(), 2);
        // Divergent suffix.
        assert!(matches!(l.truncate_suffix(2, 15), Reply::State(_)));
        assert_eq!(l.state().next, 15);
        assert!(matches!(l.truncate_suffix(2, 10), Reply::State(_)));
        assert_eq!(l.state().history, vec![(0, 1)]);
        assert_eq!(l.append(3, 10, 1, 3, false, &entries(100..105)), Reply::Appended { next: 15 });
        // Prefix truncation drops whole segments only.
        assert!(matches!(l.truncate_prefix(3, 12), Reply::State(_)));
        assert_eq!(l.state().hot_first, 12);
        assert_eq!(read_all(&l, 0)[0], "e102");
        drop(l);
        let l = LogDir::open(dir.path(), "q/%2F/a/1", true).unwrap();
        let s = l.state();
        assert_eq!((s.epoch, s.trim, s.next), (3, 12, 15));
        assert_eq!(read_all(&l, 12), ["e102", "e103", "e104"]);
        assert_eq!(list_logs(dir.path()).unwrap(), vec!["q/%2F/a/1".to_string()]);
    }

    #[test]
    fn reset_and_tiered() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = LogDir::open(dir.path(), "s", false).unwrap();
        assert_eq!(l.append(1, 0, 0, 1, false, &entries(0..5)), Reply::Appended { next: 5 });
        assert_eq!(l.append(1, 5, 1, 1, true, &entries(5..10)), Reply::Appended { next: 10 });
        assert!(matches!(l.tiered(1, 7), Reply::State(_)));
        assert_eq!(l.state().hot_first, 5);
        assert_eq!(l.read(2, 10, 100), Reply::NotHere { hot_first: 5 });
        assert!(matches!(l.reset(2, 40, 2, 30, 40), Reply::State(_)));
        let s = l.state();
        assert_eq!((s.next, s.trim, s.tiered, s.last_epoch()), (40, 30, 40, 2));
        assert_eq!(l.append(2, 40, 2, 2, false, &entries(0..2)), Reply::Appended { next: 42 });
        drop(l);
        let l = LogDir::open(dir.path(), "s", false).unwrap();
        assert_eq!(l.state().next, 42);
        assert_eq!(read_all(&l, 40), ["e0", "e1"]);
    }
}
