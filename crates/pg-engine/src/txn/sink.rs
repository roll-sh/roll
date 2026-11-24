//! A bulk load's sink (`TxnState::open_sink`): its fresh writes kept as bytes and written as
//! sorted runs, one per segment (a table's rows, or one index's entries: the keys' first 8
//! bytes), so that the runs of a load don't overlap and each goes into the store as it is at
//! commit (`Store::alone_at_bottom`), not through a compaction that merges them. A segment
//! whose keys come in order (a table's rows by row id, an index over an ascending column)
//! goes to a run at each flush as it came; one whose keys come in no order (an index over
//! another column), or behind a run of it written already, waits in sorted chunks of plain
//! bytes (`Chunk`) until the sink closes, and goes in then as one run merged from them.

use super::spill::Runs;
use crate::error::{Error, Result, code};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Fresh writes of a bulk load (`TxnState::open_sink`): keys and values back to back in
/// `data`, each (offset, key length, value length) in `ents`.
pub(super) struct Sink {
    pub(super) data: Vec<u8>,
    pub(super) ents: Vec<(u32, u32, u32)>,
    /// They may go to a run (`TxnState::sink_runs_ok`).
    pub(super) runs_ok: bool,
    /// The buffer's size (`txn_spill_bytes`): `data`'s room once it outgrows its first.
    room: usize,
    /// What earlier flushes did with each segment.
    segs: Vec<Seg>,
    /// Chunk files made, for their names.
    chunks_made: u64,
}

/// A segment's keys so far (`Sink::segs`).
struct Seg {
    prefix: [u8; 8],
    /// Its last key in a run written as it came: the next flush's keys of it go to a run of
    /// their own only after it.
    last: Vec<u8>,
    /// Its keys wait for the sink's close (`chunks`, then the last flush's).
    kept: bool,
    chunks: Vec<Chunk>,
}

/// A kept segment's entries of one flush in key order, in a file of the transaction's spill
/// directory: each entry its key's and value's lengths (4 bytes each, little-endian), then
/// the key and the value. Removed when dropped.
struct Chunk {
    path: PathBuf,
}

impl Drop for Chunk {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Chunks a kept segment collects before they are merged into one (`merge_each`).
const CHUNK_FAN: usize = 32;

/// Bytes a chunk file is written and read through.
const CHUNK_IO: usize = 1 << 20;

/// A sink's first room: grown to the whole buffer's size once (`Sink::reserve`), so a small
/// load takes little and a big one isn't copied as it doubles.
const FIRST_ROOM: usize = 1 << 20;

fn io_err(e: std::io::Error) -> Error {
    Error::new(code::IO_ERROR, format!("could not write a bulk load's sorted entries: {e}"))
}

impl Sink {
    pub(super) fn new(room: usize) -> Sink {
        let first = room.min(FIRST_ROOM);
        Sink {
            data: Vec::with_capacity(first),
            ents: Vec::with_capacity(first / 64),
            runs_ok: false,
            room,
            segs: Vec::new(),
            chunks_made: 0,
        }
    }

    /// Room for `n` more bytes: past the first room, the whole buffer's at once.
    #[inline]
    fn reserve(&mut self, n: usize) {
        if self.data.len() + n > self.data.capacity() && self.data.capacity() < self.room {
            self.grow(n);
        }
    }

    #[cold]
    fn grow(&mut self, n: usize) {
        self.data.reserve_exact(self.room.max(self.data.len() + n) - self.data.len());
        self.ents.reserve_exact((self.room / 64).saturating_sub(self.ents.len()));
    }

    /// The next chunk file's path in `dir`.
    fn chunk_path(&mut self, dir: &Path) -> PathBuf {
        self.chunks_made += 1;
        dir.join(format!("sink-{}.chunk", self.chunks_made))
    }

    pub(super) fn push(&mut self, k: &[u8], v: &[u8]) {
        self.reserve(k.len() + v.len());
        self.ents.push((self.data.len() as u32, k.len() as u32, v.len() as u32));
        self.data.extend_from_slice(k);
        self.data.extend_from_slice(v);
    }

    /// `push` of a value that is `head` then `rest`.
    pub(super) fn push_parts(&mut self, k: &[u8], head: &[u8], rest: &[u8]) {
        self.reserve(k.len() + head.len() + rest.len());
        self.ents.push((self.data.len() as u32, k.len() as u32, (head.len() + rest.len()) as u32));
        self.data.extend_from_slice(k);
        self.data.extend_from_slice(head);
        self.data.extend_from_slice(rest);
    }

    /// Whether the close's flush writes runs however few entries it holds: an earlier flush
    /// wrote runs, and this one has entries or chunks to write. (Left to the buffer, they
    /// would go to a run at commit, `link_fresh_buffer`, spanning every segment.)
    pub(super) fn close_writes_runs(&self) -> bool {
        !self.segs.is_empty() && (!self.ents.is_empty() || self.segs.iter().any(|g| !g.chunks.is_empty()))
    }

    /// Write the entries as runs of command `cid` at journal place `ts` into `runs` (whose
    /// directory, `dir`, takes the chunks): one a segment, or, unless `last`, the out-of-order
    /// ones kept in chunks. Returns how many runs. (Keys whose prefixes can't be told apart,
    /// `sink_prefixes`: one run of them all, sorted.)
    pub(super) fn flush_runs(&mut self, runs: &mut Runs, dir: &Path, cid: u32, ts: u64, last: bool) -> Result<usize> {
        let data = std::mem::take(&mut self.data);
        let ents = std::mem::take(&mut self.ents);
        let mut written = 0;
        let r = match sink_prefixes(&ents, &data) {
            Some(prefixes) => self.flush_segments(runs, dir, cid, ts, last, &prefixes, &data, &ents, &mut written),
            None => self.flush_whole(runs, cid, ts, last, &data, &ents, &mut written),
        };
        self.data = data;
        self.data.clear();
        self.ents = ents;
        self.ents.clear();
        r.map(|()| written)
    }

    /// `flush_runs` of keys of more than 255 prefixes, or under 8 bytes: one run of them
    /// all, by their first 16 bytes then the keys; `last`, each kept segment's own after it.
    #[allow(clippy::too_many_arguments)]
    fn flush_whole(&mut self, runs: &mut Runs, cid: u32, ts: u64, last: bool, data: &[u8], ents: &[(u32, u32, u32)], written: &mut usize) -> Result<()> {
        let key = |e: &(u32, u32, u32)| &data[e.0 as usize..(e.0 + e.1) as usize];
        let words = |k: &[u8]| {
            let mut b = [0u8; 16];
            let n = k.len().min(16);
            b[..n].copy_from_slice(&k[..n]);
            [u64::from_be_bytes(b[..8].try_into().unwrap()), u64::from_be_bytes(b[8..].try_into().unwrap())]
        };
        let mut order: Vec<([u64; 2], u32)> = ents.iter().enumerate().map(|(i, e)| (words(key(e)), i as u32)).collect();
        crate::sort::radix_sort_by(&mut order, |o| o.0, |a, b| a.0.cmp(&b.0).then_with(|| key(&ents[a.1 as usize]).cmp(key(&ents[b.1 as usize]))));
        debug_assert!(order.windows(2).all(|w| key(&ents[w[0].1 as usize]) < key(&ents[w[1].1 as usize])), "the sink's keys in order");
        let mut items = order.iter().map(|&(_, i)| {
            let e = ents[i as usize];
            let k = (e.0 + e.1) as usize;
            (&data[e.0 as usize..k], [&data[k..k + e.2 as usize], &[][..]])
        });
        runs.write_fresh(&mut items, cid, ts, false)?;
        *written += 1;
        if last {
            self.close_kept(runs, cid, ts, written)?;
        }
        Ok(())
    }

    /// At the close, the kept segments' chunks not merged yet, each segment's into its runs.
    fn close_kept(&mut self, runs: &mut Runs, cid: u32, ts: u64, written: &mut usize) -> Result<()> {
        for seg in std::mem::take(&mut self.segs) {
            if !seg.chunks.is_empty() {
                let index = seg.prefix[4..] != [0; 4];
                *written += merge_into_runs(runs, seg.chunks, &mut std::iter::empty(), cid, ts, index, self.room)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn flush_segments(
        &mut self,
        runs: &mut Runs,
        dir: &Path,
        cid: u32,
        ts: u64,
        last: bool,
        prefixes: &[[u8; 8]],
        data: &[u8],
        ents: &[(u32, u32, u32)],
        written: &mut usize,
    ) -> Result<()> {
        let key = |i: u32| {
            let e = ents[i as usize];
            &data[e.0 as usize..(e.0 + e.1) as usize]
        };
        let item = |i: u32| {
            let e = ents[i as usize];
            let k = (e.0 + e.1) as usize;
            (&data[e.0 as usize..k], [&data[k..k + e.2 as usize], &[][..]])
        };
        // (The entries by segment, each segment's in the order they came: a counting sort of
        // their prefixes' ranks. One segment, a table's rows alone: as they are.)
        let mut starts = vec![0usize; prefixes.len() + 1];
        let by: Vec<u32> = if let [_] = prefixes {
            starts[1] = ents.len();
            (0..ents.len() as u32).collect()
        } else {
            let ranks: Vec<u8> = ents
                .iter()
                .map(|e| {
                    let p: [u8; 8] = data[e.0 as usize..e.0 as usize + 8].try_into().expect("a prefix");
                    let r = prefixes.binary_search(&p).expect("a ranked prefix");
                    starts[r + 1] += 1;
                    r as u8
                })
                .collect();
            for r in 0..prefixes.len() {
                starts[r + 1] += starts[r];
            }
            let mut at = starts.clone();
            let mut by = vec![0u32; ents.len()];
            for (i, &r) in ranks.iter().enumerate() {
                by[at[r as usize]] = i as u32;
                at[r as usize] += 1;
            }
            by
        };
        for (r, prefix) in prefixes.iter().enumerate() {
            let idx = &by[starts[r]..starts[r + 1]];
            if idx.is_empty() {
                continue;
            }
            let g = match self.segs.iter().position(|g| g.prefix == *prefix) {
                Some(g) => g,
                None => {
                    self.segs.push(Seg { prefix: *prefix, last: Vec::new(), kept: false, chunks: Vec::new() });
                    self.segs.len() - 1
                }
            };
            let seg = &mut self.segs[g];
            if !seg.kept {
                // (Each key after the one before: by the words after their prefix, the keys
                // where those tie, as the sort compares them.)
                let mut ordered = seg.last.is_empty() || key(idx[0]) > seg.last.as_slice();
                let mut prev = words_after(key(idx[0]));
                for w in idx.windows(2) {
                    if !ordered {
                        break;
                    }
                    let cur = words_after(key(w[1]));
                    ordered = cur > prev || cur == prev && key(w[1]) > key(w[0]);
                    prev = cur;
                }
                seg.kept = !ordered;
            }
            let index = prefix[4..] != [0; 4];
            if !seg.kept {
                let mut items = idx.iter().map(|&i| item(i));
                runs.write_fresh(&mut items, cid, ts, index)?;
                *written += 1;
                seg.last.clear();
                seg.last.extend_from_slice(key(*idx.last().expect("an entry")));
                continue;
            }
            // (By the 16 bytes after the prefix, a byte at a time, then the keys where those
            // tie. Zeros past a key's end order as its end does. Keys are unique: one order.)
            let mut order: Vec<([u64; 2], u32)> = idx.iter().map(|&i| (words_after(key(i)), i)).collect();
            crate::sort::radix_sort_by(&mut order, |o| o.0, |a, b| a.0.cmp(&b.0).then_with(|| key(a.1).cmp(key(b.1))));
            debug_assert!(order.windows(2).all(|w| key(w[0].1) < key(w[1].1)), "the sink's keys in order");
            if last {
                let chunks = std::mem::take(&mut seg.chunks);
                let mut mem = order.iter().map(|o| item(o.1));
                *written += merge_into_runs(runs, chunks, &mut mem, cid, ts, index, self.room)?;
            } else {
                let mut w = ChunkWriter::create(dir, self.chunk_path(dir))?;
                for o in &order {
                    let (k, [v, _]) = item(o.1);
                    w.push(k, v)?;
                }
                let chunk = w.finish()?;
                self.segs[g].chunks.push(chunk);
                // (A load of very many flushes: their chunks merged into one, so the close's
                // merge reads at most `CHUNK_FAN` files at once.)
                if self.segs[g].chunks.len() >= CHUNK_FAN {
                    let chunks = std::mem::take(&mut self.segs[g].chunks);
                    let mut w = ChunkWriter::create(dir, self.chunk_path(dir))?;
                    merge_each(chunks, &mut std::iter::empty(), |k, v| w.push(k, v))?;
                    let merged = w.finish()?;
                    self.segs[g].chunks.push(merged);
                }
            }
        }
        if last {
            // (Kept segments without entries in this flush: their chunks alone.)
            self.close_kept(runs, cid, ts, written)?;
        }
        Ok(())
    }
}

/// The 16 bytes after a key's 8-byte prefix as two words, the first the more significant
/// (zeros past its end).
#[inline]
fn words_after(k: &[u8]) -> [u64; 2] {
    let k = &k[8..];
    // (Whole words read as such: a row's key has one after its prefix, an index entry's two
    // or more. Only a short tail is copied.)
    let word = |b: &[u8]| match b.get(..8) {
        Some(w) => u64::from_be_bytes(w.try_into().unwrap()),
        None if b.is_empty() => 0,
        None => {
            let mut w = [0u8; 8];
            w[..b.len()].copy_from_slice(b);
            u64::from_be_bytes(w)
        }
    };
    [word(k), word(k.get(8..).unwrap_or(&[]))]
}

/// The distinct first 8 bytes (table and index) of a sink's keys, in order (None: a key
/// shorter, or more than 255 of them).
pub(super) fn sink_prefixes(ents: &[(u32, u32, u32)], data: &[u8]) -> Option<Vec<[u8; 8]>> {
    let mut p: Vec<[u8; 8]> = Vec::new();
    let mut last = None;
    for e in ents {
        let k = data.get(e.0 as usize..(e.0 + e.1) as usize)?;
        let q: [u8; 8] = k.get(..8)?.try_into().ok()?;
        if last == Some(q) {
            continue;
        }
        last = Some(q);
        if !p.contains(&q) {
            if p.len() == 255 {
                return None;
            }
            p.push(q);
        }
    }
    p.sort_unstable();
    Some(p)
}

/// A chunk being written: each entry as `Chunk` lays it out.
struct ChunkWriter {
    w: BufWriter<std::fs::File>,
    chunk: Chunk,
}

impl ChunkWriter {
    fn create(dir: &Path, path: PathBuf) -> Result<ChunkWriter> {
        crate::storage::durable::create_dir_all(dir)?;
        let file = std::fs::File::create(&path).map_err(io_err)?;
        // (The chunk made before the first write: a failed one removes the file.)
        Ok(ChunkWriter { w: BufWriter::with_capacity(CHUNK_IO, file), chunk: Chunk { path } })
    }

    fn push(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
        self.w.write_all(&(k.len() as u32).to_le_bytes()).map_err(io_err)?;
        self.w.write_all(&(v.len() as u32).to_le_bytes()).map_err(io_err)?;
        self.w.write_all(k).map_err(io_err)?;
        self.w.write_all(v).map_err(io_err)
    }

    fn finish(mut self) -> Result<Chunk> {
        self.w.flush().map_err(io_err)?;
        Ok(self.chunk)
    }
}

/// A chunk read back in order, its current entry in `key` and `val`.
struct ChunkReader {
    r: BufReader<std::fs::File>,
    key: Vec<u8>,
    val: Vec<u8>,
    _chunk: Chunk,
}

impl ChunkReader {
    fn open(chunk: Chunk) -> Result<ChunkReader> {
        let file = std::fs::File::open(&chunk.path).map_err(io_err)?;
        Ok(ChunkReader { r: BufReader::with_capacity(CHUNK_IO, file), key: Vec::new(), val: Vec::new(), _chunk: chunk })
    }

    /// The next entry into `key` and `val`: false at the end.
    fn next(&mut self) -> Result<bool> {
        let mut lens = [0u8; 8];
        match self.r.read_exact(&mut lens) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(io_err(e)),
        }
        let (k, v) = (u32::from_le_bytes(lens[..4].try_into().unwrap()) as usize, u32::from_le_bytes(lens[4..].try_into().unwrap()) as usize);
        self.key.resize(k, 0);
        self.r.read_exact(&mut self.key).map_err(io_err)?;
        self.val.resize(v, 0);
        self.r.read_exact(&mut self.val).map_err(io_err)?;
        Ok(true)
    }
}

/// `chunks` and `mem` (each in key order, keys unique) merged: each entry, in key order, to
/// `f`. (The least key among the chunks' current entries and the memory's by a pass over
/// them: at most `CHUNK_FAN` chunks.)
fn merge_each<'a>(
    chunks: Vec<Chunk>,
    mem: &mut impl Iterator<Item = (&'a [u8], [&'a [u8]; 2])>,
    mut f: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let mut readers = Vec::with_capacity(chunks.len());
    for c in chunks {
        let mut r = ChunkReader::open(c)?;
        if r.next()? {
            readers.push(r);
        }
    }
    let mut head = mem.next();
    loop {
        let mut least: Option<usize> = None;
        for (i, r) in readers.iter().enumerate() {
            if least.is_none_or(|l| r.key < readers[l].key) {
                least = Some(i);
            }
        }
        let from_mem = match (&head, least) {
            (None, None) => return Ok(()),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((k, _)), Some(l)) => *k < readers[l].key.as_slice(),
        };
        if from_mem {
            let (k, [v, rest]) = head.take().expect("an entry");
            debug_assert!(rest.is_empty());
            f(k, v)?;
            head = mem.next();
        } else {
            let l = least.expect("a chunk");
            let r = &mut readers[l];
            f(&r.key, &r.val)?;
            if !r.next()? {
                readers.swap_remove(l);
            }
        }
    }
}

/// One segment's `chunks` and its last entries (`mem`, in key order) merged into runs of
/// command `cid` at journal place `ts`, a new one past each `room` bytes of entries (the
/// store's files are about that size: each goes to L1 as it is). Returns how many.
#[allow(clippy::too_many_arguments)]
fn merge_into_runs<'a>(
    runs: &mut Runs,
    chunks: Vec<Chunk>,
    mem: &mut impl Iterator<Item = (&'a [u8], [&'a [u8]; 2])>,
    cid: u32,
    ts: u64,
    index: bool,
    room: usize,
) -> Result<usize> {
    let mut run = runs.fresh_run(cid, ts, index)?;
    let (mut bytes, mut made) = (0usize, 1usize);
    merge_each(chunks, mem, |k, v| {
        if bytes >= room {
            let next = runs.fresh_run(cid, ts, index)?;
            runs.add_fresh_run(std::mem::replace(&mut run, next))?;
            made += 1;
            bytes = 0;
        }
        bytes += k.len() + v.len();
        run.push(k, [v, &[]])
    })?;
    runs.add_fresh_run(run)?;
    Ok(made)
}
