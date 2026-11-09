//! Keepers: storage nodes holding the hot tail of every log on local disk (EBS gp3 in
//! production), one per availability zone.
//!
//! A keeper is deliberately simple: it stores what writers send, refuses appends from stale
//! epochs or that don't continue its log exactly (replying with its tail so the writer can
//! repair it), and fsyncs before acknowledging. All replication logic — quorums, recovery,
//! repair, tiering — lives in the writer ([`crate::QuorumStore`]).
//!
//! Logs are sharded over a few blocking threads by key. A shard drains every queued request,
//! applies them in order, makes the batch durable and only then replies to the appends: group
//! commit across connections and logs. With fsync on, each shard has a WAL (`dir/wal/<shard>`)
//! so a batch costs one fsync however many logs it touched; segments are fsynced in the
//! background when the WAL rotates, and on start-up whatever is left in the WALs is replayed.

mod store;

use crate::proto::{self, Reply, Request};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use mq_core::transport::{Stream, Transport};
use mq_core::wal::{WalWriter, frame};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use store::LogDir;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

/// Requests one connection may have in flight before the keeper stops reading from it.
const CONN_INFLIGHT: usize = 128;
/// Requests one shard drains per group commit.
const GROUP_MAX: usize = 1024;
/// WAL file size per shard that triggers a checkpoint.
const WAL_LIMIT: u64 = 256 << 20;

#[derive(Clone, Debug)]
pub struct KeeperConfig {
    /// Data directory (logs live under `dir/logs`).
    pub dir: PathBuf,
    /// Availability zone, reported to operators and used by writers to prefer local reads.
    pub az: String,
    /// fsync before acknowledging appends. Only disable for tests and throwaway environments.
    pub fsync: bool,
    /// Blocking I/O threads; logs are spread over them by key.
    pub shards: usize,
    /// TLS and the shared secret writers must present (see [`mq_core::transport`]).
    pub transport: Arc<Transport>,
}

impl Default for KeeperConfig {
    fn default() -> Self {
        KeeperConfig { dir: PathBuf::from("keeper-data"), az: String::new(), fsync: true, shards: 8, transport: Transport::plain("keeper") }
    }
}

/// Counters a keeper exposes.
#[derive(Debug, Default)]
pub struct KeeperStats {
    pub requests: AtomicU64,
    pub appends: AtomicU64,
    pub appended_bytes: AtomicU64,
    pub fsyncs: AtomicU64,
}

struct Job {
    id: u64,
    req: Request,
    out: mpsc::UnboundedSender<Bytes>,
    _permit: OwnedSemaphorePermit,
}

/// A running keeper.
pub struct Keeper {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    pub stats: Arc<KeeperStats>,
}

/// Run a keeper on `listen` until the process exits.
pub async fn serve(listen: SocketAddr, config: KeeperConfig) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    let k = Keeper::start(listener, config).await?;
    info!(addr = %k.local_addr(), "keeper listening");
    k.task.await?;
    Ok(())
}

impl Keeper {
    pub async fn start(listener: TcpListener, config: KeeperConfig) -> anyhow::Result<Keeper> {
        let root = config.dir.join("logs");
        std::fs::create_dir_all(&root)?;
        let wal_root = config.dir.join("wal");
        let (r, w) = (root.clone(), wal_root.clone());
        tokio::task::spawn_blocking(move || recover(&r, &w)).await??;
        let addr = listener.local_addr()?;
        let stats = Arc::new(KeeperStats::default());
        let (ckpt_tx, ckpt_rx) = std::sync::mpsc::channel::<Checkpoint>();
        let mut shards = Vec::new();
        let mut threads = vec![std::thread::Builder::new().name("keeper-checkpoint".into()).spawn(move || checkpoints(ckpt_rx))?];
        for i in 0..config.shards.max(1) {
            let (tx, rx) = mpsc::channel::<Job>(4096);
            let (root, fsync, stats) = (root.clone(), config.fsync, stats.clone());
            let wal = if fsync { Some((WalWriter::open(&wal_root.join(i.to_string()))?.0, ckpt_tx.clone())) } else { None };
            threads.push(std::thread::Builder::new().name(format!("keeper-shard-{i}")).spawn(move || shard(rx, &root, fsync, &stats, wal))?);
            shards.push(tx);
        }
        drop(ckpt_tx);
        let (stop, mut stopped) = watch::channel(false);
        let shards = Arc::new(shards);
        let st = stats.clone();
        let transport = config.transport.clone();
        let task = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                    r = listener.accept() => match r {
                        Ok((sock, peer)) => {
                            debug!(%peer, "keeper connection");
                            let (transport, shards, root, st) = (transport.clone(), shards.clone(), root.clone(), st.clone());
                            conns.spawn(async move {
                                match transport.accept(sock).await {
                                    Ok(s) => connection(s, shards, root, st).await,
                                    Err(e) => warn!(%peer, error = format!("{e:#}"), "keeper connection refused"),
                                }
                            });
                        }
                        Err(e) => {
                            warn!(error = %e, "keeper accept failed");
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    },
                }
            }
            conns.shutdown().await;
            drop(shards);
            // Shard threads exit once every sender is gone, after finishing (and syncing) their batch.
            let _ = tokio::task::spawn_blocking(move || threads.into_iter().for_each(|t| drop(t.join()))).await;
        });
        Ok(Keeper { addr, stop, task, stats })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Close the listener and every connection, and wait for in-flight writes to finish.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

fn shard_of(key: &str, n: usize) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    (h % n as u64) as usize
}

async fn connection(sock: Stream, shards: Arc<Vec<mpsc::Sender<Job>>>, root: PathBuf, stats: Arc<KeeperStats>) {
    let (rd, mut wr) = sock.into_split();
    let mut rd = tokio::io::BufReader::with_capacity(256 << 10, rd);
    let (out, mut out_rx) = mpsc::unbounded_channel::<Bytes>();
    let permits = Arc::new(Semaphore::new(CONN_INFLIGHT));
    // What the writer on the other end reads: the features of its first Ping, which comes
    // before anything else (a later Ping is only a liveness check).
    let peer = std::sync::atomic::AtomicU8::new(0);
    let reader = async {
        let mut first = true;
        loop {
            let body = match proto::read_frame(&mut rd).await {
                Ok(Some(b)) => b,
                Ok(None) => break,
                Err(e) => {
                    debug!(error = %e, "keeper connection read failed");
                    break;
                }
            };
            let (id, req) = match proto::inflate(body).and_then(proto::decode_request) {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "bad keeper request");
                    break;
                }
            };
            stats.requests.fetch_add(1, Ordering::Relaxed);
            let was_first = std::mem::replace(&mut first, false);
            let Ok(permit) = permits.clone().acquire_owned().await else { break };
            match req.log().map(|l| shard_of(l, shards.len())) {
                Some(s) => {
                    if shards[s].send(Job { id, req, out: out.clone(), _permit: permit }).await.is_err() {
                        break;
                    }
                }
                None => {
                    let reply = match req {
                        Request::ListLogs => {
                            let root = root.clone();
                            match tokio::task::spawn_blocking(move || store::list_logs(&root)).await {
                                Ok(Ok(logs)) => Reply::Logs(logs),
                                Ok(Err(e)) => Reply::Error(e.to_string()),
                                Err(e) => Reply::Error(e.to_string()),
                            }
                        }
                        Request::Ping { features } => {
                            if was_first {
                                peer.store(features, Ordering::Release);
                            }
                            Reply::Pong { features: proto::FEATURES }
                        }
                        _ => Reply::Pong { features: proto::FEATURES },
                    };
                    let _ = out.send(proto::encode_reply(id, &reply));
                }
            }
        }
    };
    let writer = async {
        let mut buf = Vec::with_capacity(64 << 10);
        let mut zstd = proto::Compressor::default();
        let mut wire = |f: Bytes| if peer.load(Ordering::Acquire) & proto::FEATURE_ZSTD != 0 { zstd.frame(f) } else { f };
        while let Some(f) = out_rx.recv().await {
            buf.clear();
            buf.extend_from_slice(&wire(f));
            while buf.len() < (1 << 20)
                && let Ok(f) = out_rx.try_recv()
            {
                buf.extend_from_slice(&wire(f));
            }
            if wr.write_all(&buf).await.is_err() || wr.flush().await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

/// Segment handles to fsync, after which the WAL file they cover can go.
struct Checkpoint {
    files: Vec<File>,
    wal: PathBuf,
}

fn checkpoints(rx: std::sync::mpsc::Receiver<Checkpoint>) {
    while let Ok(c) = rx.recv() {
        match c.files.iter().try_for_each(|f| f.sync_data()) {
            Ok(()) => {
                if let Err(e) = std::fs::remove_file(&c.wal) {
                    warn!(wal = %c.wal.display(), error = %e, "removing a checkpointed WAL file");
                } else if let Some(d) = c.wal.parent().and_then(|d| File::open(d).ok()) {
                    let _ = d.sync_all();
                }
            }
            // Keep the WAL file; the next start replays it.
            Err(e) => warn!(wal = %c.wal.display(), error = %e, "checkpoint fsync failed"),
        }
    }
}

fn wal_record(buf: &mut BytesMut, key: &str, gen_: u64, first: u64, entries: &[Bytes]) {
    let mut rec = BytesMut::with_capacity(22 + key.len() + entries.iter().map(|e| 4 + e.len()).sum::<usize>());
    rec.put_u16(key.len() as u16);
    rec.put_slice(key.as_bytes());
    rec.put_u64(gen_);
    rec.put_u64(first);
    rec.put_u32(entries.len() as u32);
    for e in entries {
        rec.put_u32(e.len() as u32);
        rec.put_slice(e);
    }
    frame(buf, &rec);
}

fn decode_wal_record(mut b: Bytes) -> Option<(String, u64, u64, Vec<Bytes>)> {
    let need = |b: &Bytes, n: usize| (b.remaining() >= n).then_some(());
    need(&b, 2)?;
    let klen = b.get_u16() as usize;
    need(&b, klen + 20)?;
    let key = String::from_utf8(b.split_to(klen).to_vec()).ok()?;
    let (gen_, first, n) = (b.get_u64(), b.get_u64(), b.get_u32() as usize);
    let mut entries = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        need(&b, 4)?;
        let len = b.get_u32() as usize;
        need(&b, len)?;
        entries.push(b.split_to(len));
    }
    Some((key, gen_, first, entries))
}

/// Replay every shard WAL left by the last run into the logs, make them durable and drop the
/// WALs. Runs before any shard starts, so the shard count may change between runs.
fn recover(root: &Path, wal_root: &Path) -> anyhow::Result<()> {
    let Ok(dirs) = std::fs::read_dir(wal_root) else { return Ok(()) };
    let mut dirs: Vec<PathBuf> = dirs.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let (writer, records, _) = WalWriter::open(&dir)?;
        drop(writer);
        let mut logs: HashMap<String, LogDir> = HashMap::new();
        let mut bad: HashSet<String> = HashSet::new();
        let n = records.len();
        for r in records {
            let Some((key, gen_, first, entries)) = decode_wal_record(r) else {
                anyhow::bail!("unreadable record in {}", dir.display());
            };
            if bad.contains(&key) {
                continue;
            }
            let log = match logs.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => v.insert(LogDir::open(root, &key, true)?),
            };
            if let Err(e) = log.replay(gen_, first, &entries) {
                warn!(log = %key, error = ?e, "WAL replay stopped for this log");
                bad.insert(key);
            }
        }
        for (key, mut log) in logs {
            log.sync().map_err(|e| anyhow::anyhow!("sync {key} after WAL replay: {e}"))?;
        }
        std::fs::remove_dir_all(&dir)?;
        if n > 0 {
            info!(wal = %dir.display(), records = n, "replayed keeper WAL");
        }
    }
    if let Ok(d) = File::open(wal_root) {
        d.sync_all()?;
    }
    Ok(())
}

type ShardWal = (WalWriter, std::sync::mpsc::Sender<Checkpoint>);

fn shard(mut rx: mpsc::Receiver<Job>, root: &Path, fsync: bool, stats: &KeeperStats, mut wal: Option<ShardWal>) {
    let mut logs: HashMap<String, Result<LogDir, String>> = HashMap::new();
    let mut jobs = Vec::new();
    let mut deferred: Vec<(Job, Reply)> = Vec::new();
    let mut dirty: Vec<String> = Vec::new();
    // Logs with writes only the WAL covers, since the last rotation.
    let mut unsynced: HashSet<String> = HashSet::new();
    let mut walbuf = BytesMut::new();
    while let Some(job) = rx.blocking_recv() {
        jobs.push(job);
        while jobs.len() < GROUP_MAX
            && let Ok(j) = rx.try_recv()
        {
            jobs.push(j);
        }
        for job in jobs.drain(..) {
            let Some(key) = job.req.log().map(str::to_string) else { continue };
            let entry = logs.entry(key.clone()).or_insert_with(|| LogDir::open(root, &key, fsync).map_err(|e| e.to_string()));
            let log = match entry {
                Ok(l) => l,
                Err(e) => {
                    let _ = job.out.send(proto::encode_reply(job.id, &Reply::Error(format!("log {key} is unreadable: {e}"))));
                    continue;
                }
            };
            let reply = match &job.req {
                Request::Fence { epoch, strict, .. } => log.fence(*epoch, *strict),
                Request::Append { epoch, first, prev_epoch, entry_epoch, seal, entries, .. } => {
                    let r = log.append(*epoch, *first, *prev_epoch, *entry_epoch, *seal, entries);
                    if matches!(r, Reply::Appended { .. }) && !entries.is_empty() {
                        stats.appends.fetch_add(1, Ordering::Relaxed);
                        stats.appended_bytes.fetch_add(entries.iter().map(|e| e.len() as u64).sum(), Ordering::Relaxed);
                        if wal.is_some() {
                            wal_record(&mut walbuf, &key, log.wal_gen(), *first, entries);
                        }
                        if !dirty.contains(&key) {
                            dirty.push(key.clone());
                        }
                    }
                    r
                }
                Request::Read { from, max, max_bytes, .. } => log.read(*from, *max as usize, *max_bytes as usize),
                Request::State { .. } => Reply::State(log.state()),
                Request::TruncatePrefix { epoch, before, .. } => log.truncate_prefix(*epoch, *before),
                Request::TruncateSuffix { epoch, from, .. } => log.truncate_suffix(*epoch, *from),
                Request::Reset { epoch, at, prev_epoch, trim, tiered, .. } => log.reset(*epoch, *at, *prev_epoch, *trim, *tiered),
                Request::Tiered { epoch, upto, .. } => log.tiered(*epoch, *upto),
                Request::Destroy { epoch, .. } => {
                    let r = log.destroy(*epoch);
                    if matches!(r, Reply::State(_)) {
                        logs.remove(&key);
                    }
                    r
                }
                Request::ListLogs | Request::Ping { .. } => Reply::Pong { features: proto::FEATURES },
            };
            if matches!(reply, Reply::Error(_)) {
                // After a failed write the in-memory view may not match the disk; reload it.
                logs.remove(&key);
            }
            deferred.push((job, reply));
        }
        // Group commit, then reply: nothing unsynced is ever reported.
        let mut failed: Vec<(String, String)> = Vec::new();
        match &mut wal {
            Some((w, ckpt)) => {
                if !walbuf.is_empty() {
                    stats.fsyncs.fetch_add(1, Ordering::Relaxed);
                    let res = w.write(&walbuf).and_then(|_| w.sync());
                    walbuf.clear();
                    match res {
                        Ok(()) => unsynced.extend(dirty.drain(..)),
                        Err(e) => {
                            warn!(error = %e, "keeper WAL write failed");
                            for key in dirty.drain(..) {
                                // The segment write happened; make it durable on its own.
                                let synced = logs.get_mut(&key).and_then(|l| l.as_mut().ok()).map(|l| l.sync());
                                if !matches!(synced, Some(Ok(()))) {
                                    failed.push((key.clone(), e.to_string()));
                                    logs.remove(&key);
                                }
                            }
                            // The file's tail is unknown now; continue in a fresh one.
                            if let Ok(old) = w.rotate() {
                                let files = unsynced.drain().filter_map(|k| logs.get_mut(&k)?.as_mut().ok()?.take_unsynced()).collect();
                                let _ = ckpt.send(Checkpoint { files, wal: old });
                            }
                        }
                    }
                }
                if w.size() >= WAL_LIMIT {
                    match w.rotate() {
                        Ok(old) => {
                            let files = unsynced.drain().filter_map(|k| logs.get_mut(&k)?.as_mut().ok()?.take_unsynced()).collect();
                            let _ = ckpt.send(Checkpoint { files, wal: old });
                        }
                        Err(e) => warn!(error = %e, "keeper WAL rotation failed"),
                    }
                }
            }
            None => {
                for key in dirty.drain(..) {
                    if let Some(Ok(log)) = logs.get_mut(&key) {
                        if fsync {
                            stats.fsyncs.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Err(e) = log.sync() {
                            warn!(log = %key, error = %e, "keeper fsync failed");
                            failed.push((key.clone(), e.to_string()));
                            // We can't know what reached the disk; reload from it on next use.
                            logs.remove(&key);
                        }
                    }
                }
            }
        }
        for (job, reply) in deferred.drain(..) {
            let key = job.req.log().unwrap_or_default();
            let reply = match failed.iter().find(|(k, _)| k == key) {
                Some((_, e)) if matches!(reply, Reply::Appended { .. } | Reply::State(_) | Reply::Entries { .. }) => Reply::Error(format!("fsync failed: {e}")),
                _ => reply,
            };
            let _ = job.out.send(proto::encode_reply(job.id, &reply));
        }
    }
    // Shutting down: a last checkpoint covers everything the WAL holds.
    if let Some((mut w, ckpt)) = wal
        && let Ok(old) = w.rotate()
    {
        let files = unsynced.drain().filter_map(|k| logs.get_mut(&k)?.as_mut().ok()?.take_unsynced()).collect();
        let _ = ckpt.send(Checkpoint { files, wal: old });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(r: std::ops::Range<u32>) -> Vec<Bytes> {
        r.map(|i| Bytes::from(format!("e{i}"))).collect()
    }

    fn read_all(l: &LogDir) -> Vec<String> {
        match l.read(0, 1000, 1 << 20) {
            Reply::Entries { entries, .. } => entries.iter().map(|e| String::from_utf8_lossy(e).into_owned()).collect(),
            other => panic!("{other:?}"),
        }
    }

    /// Append through a log and its WAL the way a shard does.
    fn append(l: &mut LogDir, w: &mut WalWriter, key: &str, epoch: u64, first: u64, es: &[Bytes]) {
        assert_eq!(l.append(epoch, first, if first == 0 { 0 } else { epoch }, epoch, false, es), Reply::Appended { next: first + es.len() as u64 });
        let mut buf = BytesMut::new();
        wal_record(&mut buf, key, l.wal_gen(), first, es);
        w.write(&buf).unwrap();
        w.sync().unwrap();
    }

    fn lose_tail(root: &Path, key: &str, keep: u64) {
        let dir = root.join(store::dir_name(key));
        let seg = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "seg")).max().unwrap();
        std::fs::OpenOptions::new().write(true).open(seg).unwrap().set_len(keep).unwrap();
    }

    #[test]
    fn wal_replay_restores_unsynced_appends() {
        let dir = tempfile::tempdir().unwrap();
        let (root, wal_root) = (dir.path().join("logs"), dir.path().join("wal"));
        let (mut w, _, _) = WalWriter::open(&wal_root.join("0")).unwrap();
        let mut l = LogDir::open(&root, "q/a", true).unwrap();
        append(&mut l, &mut w, "q/a", 1, 0, &entries(0..5));
        append(&mut l, &mut w, "q/a", 1, 5, &entries(5..9));
        std::mem::forget(l); // a crash: no sync on drop
        drop(w);
        lose_tail(&root, "q/a", 30);
        recover(&root, &wal_root).unwrap();
        let l = LogDir::open(&root, "q/a", true).unwrap();
        assert_eq!(read_all(&l), (0..9).map(|i| format!("e{i}")).collect::<Vec<_>>());
        assert!(!wal_root.join("0").exists(), "replayed WALs are removed");
    }

    #[test]
    fn wal_replay_never_undoes_a_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let (root, wal_root) = (dir.path().join("logs"), dir.path().join("wal"));
        let (mut w, _, _) = WalWriter::open(&wal_root.join("0")).unwrap();
        let mut l = LogDir::open(&root, "q/b", true).unwrap();
        append(&mut l, &mut w, "q/b", 1, 0, &entries(0..10));
        assert!(matches!(l.truncate_suffix(2, 6), Reply::State(_)));
        assert_eq!(l.append(2, 6, 1, 2, false, &entries(60..62)), Reply::Appended { next: 8 });
        let mut buf = BytesMut::new();
        wal_record(&mut buf, "q/b", l.wal_gen(), 6, &entries(60..62));
        w.write(&buf).unwrap();
        w.sync().unwrap();
        std::mem::forget(l);
        drop(w);
        recover(&root, &wal_root).unwrap();
        let l = LogDir::open(&root, "q/b", true).unwrap();
        let want: Vec<String> = (0..6).map(|i| format!("e{i}")).chain(["e60".into(), "e61".into()]).collect();
        assert_eq!(read_all(&l), want);
    }
}
