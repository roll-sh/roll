//! A write-ahead log shared by many logs, for group commit.
//!
//! A log that fsyncs its own files pays one fsync per write, so a few hundred busy queues mean
//! thousands of fsyncs a second and a long tail of queueing behind them (RabbitMQ's quorum
//! queues share one WAL for the same reason). With a [`Wal`], logs write their own files
//! without fsync and hand the same bytes to the WAL. One writer thread takes whatever is queued,
//! writes it in one go and fsyncs once for the whole batch.
//!
//! When the WAL passes its size limit it moves to a new file, and a checkpoint thread fsyncs
//! every registered log that has unsynced writes before deleting the old file. Whatever is left
//! on start-up is returned by [`WalWriter::open`] for the owner to replay. Records are opaque
//! payloads framed as `[len u32][crc32 u32][payload]`; replay stops at the first torn frame.

use crate::log::crc32;
use bytes::{BufMut, Bytes, BytesMut};
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use tokio::sync::oneshot;
use tracing::warn;

/// Records the writer drains per group commit.
const GROUP_MAX: usize = 4096;
/// Commits slower than this are logged.
const SLOW: std::time::Duration = std::time::Duration::from_millis(100);

/// The WAL's files: `<seq>.wal`, written by one owner at a time.
pub struct WalWriter {
    dir: PathBuf,
    seq: u64,
    file: File,
    size: u64,
}

impl WalWriter {
    /// Open the WAL in `dir`, returning every intact record left in it (oldest first) and the
    /// files they came from. Replay the records, make the result durable, then
    /// [`WalWriter::remove`] the files.
    pub fn open(dir: &Path) -> io::Result<(WalWriter, Vec<Bytes>, Vec<PathBuf>)> {
        std::fs::create_dir_all(dir)?;
        let mut files: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let seq = p.file_stem()?.to_str()?.parse().ok()?;
                (p.extension()? == "wal").then_some((seq, p))
            })
            .collect();
        files.sort();
        let mut records = Vec::new();
        for (_, path) in &files {
            let mut data = Vec::new();
            File::open(path)?.read_to_end(&mut data)?;
            let data = Bytes::from(data);
            let mut pos = 0usize;
            while pos + 8 <= data.len() {
                let len = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
                let crc = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap());
                if pos + 8 + len > data.len() || crc32(&data[pos + 8..pos + 8 + len]) != crc {
                    break;
                }
                records.push(data.slice(pos + 8..pos + 8 + len));
                pos += 8 + len;
            }
            if pos < data.len() {
                // Only the unsynced tail of a file can be torn; nothing after it was acknowledged.
                warn!(wal = %path.display(), at = pos, "WAL ends in a torn record");
            }
        }
        let seq = files.last().map(|(s, _)| s + 1).unwrap_or(0);
        let file = Self::create(dir, seq)?;
        let old = files.into_iter().map(|(_, p)| p).collect();
        Ok((WalWriter { dir: dir.to_path_buf(), seq, file, size: 0 }, records, old))
    }

    fn create(dir: &Path, seq: u64) -> io::Result<File> {
        let f = OpenOptions::new().create(true).append(true).open(dir.join(format!("{seq:020}.wal")))?;
        File::open(dir)?.sync_all()?;
        Ok(f)
    }

    /// Delete WAL files whose records are durable elsewhere.
    pub fn remove(&self, files: &[PathBuf]) -> io::Result<()> {
        for f in files {
            std::fs::remove_file(f).or_else(|e| if e.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(e) })?;
        }
        if !files.is_empty() {
            File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Append already framed records (see [`frame`]).
    pub fn write(&mut self, framed: &[u8]) -> io::Result<()> {
        self.file.write_all(framed)?;
        self.size += framed.len() as u64;
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Bytes written to the current file.
    pub fn size(&self) -> u64 {
        self.size
    }

    fn current(&self) -> PathBuf {
        self.dir.join(format!("{:020}.wal", self.seq))
    }

    /// Sync the current file and start a new one; returns the old file's path.
    pub fn rotate(&mut self) -> io::Result<PathBuf> {
        self.file.sync_data()?;
        let old = self.current();
        self.seq += 1;
        self.file = Self::create(&self.dir, self.seq)?;
        self.size = 0;
        Ok(old)
    }
}

/// Frame one record onto `buf`.
pub fn frame(buf: &mut BytesMut, payload: &[u8]) {
    buf.put_u32(payload.len() as u32);
    buf.put_u32(crc32(payload));
    buf.put_slice(payload);
}

/// Something with writes covered by the WAL: a checkpoint makes them durable on their own.
pub trait Checkpoint: Send + Sync {
    fn checkpoint(&self) -> io::Result<()>;
}

type Done = oneshot::Sender<io::Result<()>>;

struct Submit {
    payload: Bytes,
    done: Option<Done>,
}

/// The shared WAL: a writer thread doing group commits and a checkpoint thread.
pub struct Wal {
    tx: Mutex<Option<mpsc::Sender<Submit>>>,
    logs: Arc<Mutex<Vec<Weak<dyn Checkpoint>>>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    crashed: Arc<AtomicBool>,
}

impl Wal {
    /// Start group commits on `writer`, rotating once a file passes `limit` bytes.
    pub fn start(writer: WalWriter, limit: u64) -> io::Result<Arc<Wal>> {
        let (tx, rx) = mpsc::channel::<Submit>();
        let (ctx, crx) = mpsc::channel::<PathBuf>();
        let logs: Arc<Mutex<Vec<Weak<dyn Checkpoint>>>> = Arc::default();
        let l = logs.clone();
        let crashed = Arc::new(AtomicBool::new(false));
        let c = crashed.clone();
        let writer_thread = std::thread::Builder::new().name("wal-writer".into()).spawn(move || run_writer(writer, rx, ctx, limit, &c))?;
        let checkpoint_thread = std::thread::Builder::new().name("wal-checkpoint".into()).spawn(move || run_checkpoints(crx, l))?;
        Ok(Arc::new(Wal { tx: Mutex::new(Some(tx)), logs, threads: Mutex::new(vec![writer_thread, checkpoint_thread]), crashed }))
    }

    /// Queue a record. With `durable`, the receiver resolves once it is fsynced; otherwise it
    /// becomes durable with the next durable record.
    pub fn submit(&self, payload: Bytes, durable: bool) -> Option<oneshot::Receiver<io::Result<()>>> {
        let (done, rx) = if durable {
            let (t, r) = oneshot::channel();
            (Some(t), Some(r))
        } else {
            (None, None)
        };
        let sent = self.tx.lock().as_ref().is_some_and(|tx| tx.send(Submit { payload, done }).is_ok());
        if !sent {
            let (t, r) = oneshot::channel();
            let _ = t.send(Err(io::Error::other("WAL is stopped")));
            return Some(r);
        }
        rx
    }

    /// Have checkpoints sync `log` before dropping WAL files.
    pub fn register(&self, log: Weak<dyn Checkpoint>) {
        self.logs.lock().push(log);
    }

    /// Stop as a crash would: no final checkpoint, the WAL files stay for replay. For tests.
    #[doc(hidden)]
    pub fn crash(&self) {
        self.crashed.store(true, Ordering::Relaxed);
        self.stop();
    }

    /// Flush what's queued, checkpoint every log and stop the threads.
    pub fn stop(&self) {
        self.tx.lock().take();
        for t in self.threads.lock().drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        self.tx.get_mut().take();
    }
}

fn run_writer(mut w: WalWriter, rx: mpsc::Receiver<Submit>, checkpoints: mpsc::Sender<PathBuf>, limit: u64, crashed: &AtomicBool) {
    let mut buf = BytesMut::new();
    let mut waiting: Vec<Done> = Vec::new();
    let mut broken: Option<String> = None;
    while let Ok(first) = rx.recv() {
        buf.clear();
        let mut take = |s: Submit| {
            frame(&mut buf, &s.payload);
            waiting.extend(s.done);
        };
        take(first);
        let mut n = 1;
        while n < GROUP_MAX
            && let Ok(s) = rx.try_recv()
        {
            take(s);
            n += 1;
        }
        let res = match &broken {
            // After a failed write the file's tail is unknown; refuse everything from then on.
            Some(e) => Err(e.clone()),
            None => {
                let t = std::time::Instant::now();
                let r = w.write(&buf).and_then(|_| if waiting.is_empty() { Ok(()) } else { w.sync() }).map_err(|e| e.to_string());
                if t.elapsed() > SLOW {
                    warn!(ms = t.elapsed().as_millis() as u64, records = n, bytes = buf.len(), "slow WAL commit");
                }
                r
            }
        };
        if let Err(e) = &res
            && broken.is_none()
        {
            warn!(error = %e, "WAL write failed; refusing durable writes");
            broken = Some(e.clone());
        }
        for d in waiting.drain(..) {
            let _ = d.send(res.clone().map_err(io::Error::other));
        }
        if broken.is_none() && w.size() >= limit {
            match w.rotate() {
                Ok(old) => {
                    let _ = checkpoints.send(old);
                }
                Err(e) => warn!(error = %e, "WAL rotation failed"),
            }
        }
    }
    // Shutting down: one last checkpoint covers everything, then the file can go too.
    if crashed.load(Ordering::Relaxed) {
        return;
    }
    match (&broken, w.sync()) {
        (None, Ok(())) => {
            let _ = checkpoints.send(w.current());
        }
        (_, Err(e)) => warn!(error = %e, "final WAL sync failed"),
        _ => {}
    }
}

fn run_checkpoints(rx: mpsc::Receiver<PathBuf>, logs: Arc<Mutex<Vec<Weak<dyn Checkpoint>>>>) {
    let mut pending: Vec<PathBuf> = Vec::new();
    let dir = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
    while let Ok(old) = rx.recv() {
        pending.push(old);
        while let Ok(more) = rx.try_recv() {
            pending.push(more);
        }
        let live: Vec<Arc<dyn Checkpoint>> = {
            let mut l = logs.lock();
            l.retain(|w| w.strong_count() > 0);
            l.iter().filter_map(Weak::upgrade).collect()
        };
        let mut ok = true;
        for log in live {
            if let Err(e) = log.checkpoint() {
                warn!(error = %e, "checkpoint fsync failed; keeping the WAL");
                ok = false;
            }
        }
        if ok {
            for p in pending.drain(..) {
                if let Err(e) = std::fs::remove_file(&p) {
                    if e.kind() == io::ErrorKind::NotFound {
                        continue;
                    }
                    warn!(wal = %p.display(), error = %e, "removing a checkpointed WAL file");
                } else if let Ok(d) = File::open(dir(&p)) {
                    let _ = d.sync_all();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("mq-wal-test-{}", crate::rand_u64()))
    }

    #[test]
    fn writer_replays_intact_records_and_drops_a_torn_tail() {
        let dir = tmp();
        let (mut w, recs, old) = WalWriter::open(&dir).unwrap();
        assert!(recs.is_empty() && old.is_empty());
        let mut buf = BytesMut::new();
        for r in ["a", "b"] {
            frame(&mut buf, r.as_bytes());
        }
        w.write(&buf).unwrap();
        w.rotate().unwrap();
        buf.clear();
        frame(&mut buf, b"c");
        w.write(&buf).unwrap();
        w.write(&[0, 0, 0, 9, 1, 2, 3]).unwrap();
        w.sync().unwrap();
        drop(w);
        let (w, recs, old) = WalWriter::open(&dir).unwrap();
        assert_eq!(recs, [Bytes::from_static(b"a"), Bytes::from_static(b"b"), Bytes::from_static(b"c")]);
        assert_eq!(old.len(), 2);
        w.remove(&old).unwrap();
        drop(w);
        let (_, recs, old) = WalWriter::open(&dir).unwrap();
        assert!(recs.is_empty());
        assert_eq!(old.len(), 1, "only the empty file the last open created");
        std::fs::remove_dir_all(&dir).ok();
    }

    struct Counter(AtomicUsize);

    impl Checkpoint for Counter {
        fn checkpoint(&self) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn group_commits_checkpoint_rotated_files_away() {
        let dir = tmp();
        let (w, _, _) = WalWriter::open(&dir).unwrap();
        let wal = Wal::start(w, 256).unwrap();
        let log = Arc::new(Counter(AtomicUsize::new(0)));
        let weak: Weak<dyn Checkpoint> = Arc::downgrade(&(log.clone() as Arc<dyn Checkpoint>));
        wal.register(weak);
        let mut acks = Vec::new();
        for i in 0..200u32 {
            assert_eq!(wal.submit(Bytes::from(format!("r{i:03}")), i % 10 == 9).is_some(), i % 10 == 9);
            if i % 10 == 9 {
                acks.push(wal.submit(Bytes::from(format!("d{i:03}")), true).unwrap());
            }
        }
        for a in acks {
            a.await.unwrap().unwrap();
        }
        wal.stop();
        // Rotations that queue up together share one checkpoint, so count at least one.
        assert!(log.0.load(Ordering::Relaxed) >= 1, "rotations and the final stop checkpoint");
        let left = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(left, 0, "every WAL file was checkpointed away");
        assert!(wal.submit(Bytes::from_static(b"late"), false).is_some(), "a stopped WAL refuses writes");
        std::fs::remove_dir_all(&dir).ok();
    }
}
