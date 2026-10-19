//! Group commit for [`crate::S3ExpressStore`]: one object per broker per few milliseconds
//! instead of one per log append, so request costs follow the number of brokers rather than
//! the number of queues.
//!
//! A store with grouping on owns a *stream*, `{prefix}/_streams/{id:016x}/`, of objects
//! `{seq:020}` written with put-if-absent, strictly in order (a write waits for the one before).
//! Each object holds *records*, one per log append that came in while the previous write was in
//! flight (or within `linger`): `(log, epoch, writer, first index, entries)`. An append is
//! confirmed once its object is written.
//!
//! Logs materialize what they staged in the stream into their own objects later (see
//! `Shared::flush` in [`crate::express`]), in large batches: after `flush_after`, at
//! `flush_bytes`, or when closed. Before a log stages its first entry on a stream it puts a
//! *link* in its own directory (`{index:020}`, put-if-absent like any batch), naming the
//! stream, so whoever opens the log next knows where unmaterialized entries may be.
//!
//! Fencing works on the stream as a whole. A new owner that finds a link to a stream which
//! may still hold entries *drains* it: it puts a fence object at the stream's next sequence
//! number (the old writer's next write collides with it, so it can never confirm anything after
//! that point), copies every record before the fence into its log's directory, and marks the
//! stream drained. A writer whose stream was fenced moves to a new stream; each of its logs
//! materializes what it staged and puts a fresh link before staging again, and that link
//! collides with the new owner's fence marker if the log changed hands.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use mq_core::log::crc32;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, warn};

const RECORDS_MAGIC: u32 = 0x5251_5331; // "RQS1"
const FENCE_MAGIC: u32 = 0x5251_5346; // "RQSF"
/// `[magic u32][stream u64][nonce u64][count u32]`; a fence object is just this.
pub(crate) const STREAM_HEADER: usize = 24;

/// How appends are grouped (see the module docs).
#[derive(Clone, Debug)]
pub struct GroupConfig {
    /// How long the first append waits for company when no write is in flight.
    pub linger: Duration,
    /// Largest stream object; appends beyond wait for the next one.
    pub max_object_bytes: usize,
    /// A log materializes what it staged at least this often...
    pub flush_after: Duration,
    /// ...or once it has staged this much...
    pub flush_bytes: usize,
    /// ...or when everything staged in the store passes this (the largest logs go first).
    pub max_staged_bytes: usize,
}

impl Default for GroupConfig {
    fn default() -> Self {
        GroupConfig {
            linger: Duration::from_millis(4),
            max_object_bytes: 8 << 20,
            flush_after: Duration::from_secs(300),
            flush_bytes: 1 << 20,
            max_staged_bytes: 256 << 20,
        }
    }
}

/// Requests a store made, by kind (what S3 bills for).
#[derive(Debug, Default)]
pub struct Requests {
    pub puts: AtomicU64,
    pub gets: AtomicU64,
    pub lists: AtomicU64,
    pub deletes: AtomicU64,
}

impl Requests {
    pub(crate) fn put(&self) {
        self.puts.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn get(&self) {
        self.gets.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn list(&self) {
        self.lists.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn delete(&self) {
        self.deletes.fetch_add(1, Ordering::Relaxed);
    }
}

/// One log append, as it goes into a stream object.
pub(crate) struct Record {
    pub key: String,
    pub epoch: u64,
    pub writer: u64,
    pub first: u64,
    pub entries: Vec<Bytes>,
}

impl Record {
    fn encoded_len(&self) -> usize {
        2 + self.key.len() + 28 + self.entries.iter().map(|e| 8 + e.len()).sum::<usize>()
    }

    fn encode(&self, b: &mut BytesMut) {
        b.put_u16(self.key.len() as u16);
        b.put_slice(self.key.as_bytes());
        b.put_u64(self.epoch);
        b.put_u64(self.writer);
        b.put_u64(self.first);
        b.put_u32(self.entries.len() as u32);
        for e in &self.entries {
            b.put_u32(e.len() as u32);
            b.put_u32(crc32(e));
            b.put_slice(e);
        }
    }
}

fn header(b: &mut BytesMut, magic: u32, stream: u64, nonce: u64, count: u32) {
    b.put_u32(magic);
    b.put_u64(stream);
    b.put_u64(nonce);
    b.put_u32(count);
}

pub(crate) fn encode_fence(stream: u64, nonce: u64) -> Bytes {
    let mut b = BytesMut::with_capacity(STREAM_HEADER);
    header(&mut b, FENCE_MAGIC, stream, nonce, 0);
    b.freeze()
}

/// The records of a stream object (empty for a fence).
pub(crate) fn decode_object(mut data: Bytes) -> anyhow::Result<(u64, Vec<Record>)> {
    anyhow::ensure!(data.len() >= STREAM_HEADER, "truncated stream object");
    let magic = data.get_u32();
    let _stream = data.get_u64();
    let nonce = data.get_u64();
    let count = data.get_u32();
    if magic == FENCE_MAGIC {
        return Ok((nonce, Vec::new()));
    }
    anyhow::ensure!(magic == RECORDS_MAGIC, "not a stream object");
    let mut out = Vec::with_capacity(count as usize);
    let need = |d: &Bytes, n: usize| -> anyhow::Result<()> {
        anyhow::ensure!(d.remaining() >= n, "truncated stream object");
        Ok(())
    };
    for _ in 0..count {
        need(&data, 2)?;
        let klen = data.get_u16() as usize;
        need(&data, klen + 28)?;
        let key = String::from_utf8(data.split_to(klen).to_vec())?;
        let (epoch, writer, first, n) = (data.get_u64(), data.get_u64(), data.get_u64(), data.get_u32());
        need(&data, (n as usize).saturating_mul(8))?;
        let mut entries = Vec::with_capacity(n as usize);
        for _ in 0..n {
            need(&data, 8)?;
            let (len, crc) = (data.get_u32() as usize, data.get_u32());
            need(&data, len)?;
            let e = data.split_to(len);
            anyhow::ensure!(crc32(&e) == crc, "stream record for {key} failed its checksum");
            entries.push(e);
        }
        out.push(Record { key, epoch, writer, first, entries });
    }
    Ok((nonce, out))
}

pub(crate) fn stream_dir(root: &Path, stream: u64) -> Path {
    root.clone().join(format!("{stream:016x}"))
}

pub(crate) fn object_path(root: &Path, stream: u64, seq: u64) -> Path {
    stream_dir(root, stream).join(format!("{seq:020}"))
}

/// Why a grouped append didn't go through.
#[derive(Debug)]
pub(crate) enum GroupErr {
    /// The stream changed since the caller linked it: link the new one and retry.
    Changed,
    /// Someone fenced the stream (a new owner drained it). Nothing was written.
    Fenced,
    /// The write failed and may or may not have landed, in object `(stream, seq)` when known
    /// (release it like a successful one).
    Failed(String, Option<(u64, u64)>),
}

struct Pending {
    expect: u64,
    rec: Record,
    reply: oneshot::Sender<Result<(u64, u64), GroupErr>>,
}

struct GState {
    /// The stream being written, and its next sequence number.
    id: u64,
    next_seq: u64,
    pending: VecDeque<Pending>,
    running: bool,
    /// Records in each object whose log hasn't materialized them yet.
    refs: HashMap<(u64, u64), usize>,
    /// The newest object of the current stream, once nothing needs it: deleted when a newer
    /// one exists (the newest object is what a fencer's put has to collide after).
    idle_latest: Option<u64>,
}

/// A store's stream writer.
pub(crate) struct Group {
    pub cfg: GroupConfig,
    store: Arc<dyn ObjectStore>,
    pub root: Path,
    pub requests: Arc<Requests>,
    st: Mutex<GState>,
    /// Bytes staged by all logs of the store.
    pub staged: AtomicUsize,
}

impl Group {
    pub fn new(cfg: GroupConfig, store: Arc<dyn ObjectStore>, root: Path, requests: Arc<Requests>) -> Arc<Group> {
        let id = mq_core::rand_u64();
        Arc::new(Group {
            cfg,
            store,
            root,
            requests,
            st: Mutex::new(GState { id, next_seq: 0, pending: VecDeque::new(), running: false, refs: HashMap::new(), idle_latest: None }),
            staged: AtomicUsize::new(0),
        })
    }

    /// The stream appends go to now.
    pub fn current(&self) -> (u64, u64) {
        let st = self.st.lock();
        (st.id, st.next_seq)
    }

    /// Write `rec` in the next object of stream `expect`; returns `(stream, seq)` once it is
    /// durable. The caller must [`Group::release`] that object once it has materialized the
    /// entries (or no longer needs them).
    pub async fn submit(self: &Arc<Self>, expect: u64, rec: Record) -> Result<(u64, u64), GroupErr> {
        let (reply, rx) = oneshot::channel();
        {
            let mut st = self.st.lock();
            if st.id != expect {
                return Err(GroupErr::Changed);
            }
            st.pending.push_back(Pending { expect, rec, reply });
            if !st.running {
                st.running = true;
                let me = self.clone();
                tokio::spawn(async move { me.commit_loop().await });
            }
        }
        rx.await.unwrap_or_else(|_| Err(GroupErr::Failed("stream writer stopped".into(), None)))
    }

    /// One materialized (or abandoned) record of object `(stream, seq)`.
    pub fn release(self: &Arc<Self>, stream: u64, seq: u64) {
        let delete = {
            let mut st = self.st.lock();
            let Some(n) = st.refs.get_mut(&(stream, seq)) else { return };
            *n -= 1;
            if *n > 0 {
                return;
            }
            st.refs.remove(&(stream, seq));
            if stream == st.id && seq + 1 == st.next_seq {
                st.idle_latest = Some(seq);
                false
            } else {
                true
            }
        };
        if delete {
            self.delete(stream, seq);
        }
    }

    fn delete(self: &Arc<Self>, stream: u64, seq: u64) {
        let me = self.clone();
        tokio::spawn(async move {
            me.requests.delete();
            if let Err(e) = me.store.delete(&object_path(&me.root, stream, seq)).await
                && !matches!(e, object_store::Error::NotFound { .. })
            {
                debug!(stream = format!("{stream:016x}"), seq, error = %e, "could not delete a stream object");
            }
        });
    }

    async fn commit_loop(self: Arc<Self>) {
        let mut first = true;
        loop {
            if first {
                tokio::time::sleep(self.cfg.linger).await;
                first = false;
            }
            let (id, seq, batch) = {
                let mut st = self.st.lock();
                let mut batch = Vec::new();
                let mut bytes = STREAM_HEADER;
                while let Some(p) = st.pending.front() {
                    let len = p.rec.encoded_len();
                    if !batch.is_empty() && bytes + len > self.cfg.max_object_bytes {
                        break;
                    }
                    let p = st.pending.pop_front().unwrap();
                    if p.expect != st.id {
                        let _ = p.reply.send(Err(GroupErr::Changed));
                        continue;
                    }
                    bytes += len;
                    batch.push(p);
                }
                if batch.is_empty() {
                    st.running = false;
                    return;
                }
                (st.id, st.next_seq, batch)
            };
            let mut body = BytesMut::with_capacity(STREAM_HEADER + batch.iter().map(|p| p.rec.encoded_len()).sum::<usize>());
            header(&mut body, RECORDS_MAGIC, id, id, batch.len() as u32);
            for p in &batch {
                p.rec.encode(&mut body);
            }
            let result = self.put(id, seq, body.freeze()).await;
            let mut st = self.st.lock();
            match result {
                Ok(()) => {
                    st.next_seq = seq + 1;
                    st.refs.insert((id, seq), batch.len());
                    if let Some(prev) = st.idle_latest.take() {
                        drop(st);
                        self.delete(id, prev);
                        st = self.st.lock();
                    }
                    for p in batch {
                        let _ = p.reply.send(Ok((id, seq)));
                    }
                }
                Err(e) => {
                    // A fenced stream is done; so is one whose last write is in doubt (its
                    // sequence can't continue past an object that may or may not exist).
                    let fenced = matches!(e, GroupErr::Fenced);
                    let new = mq_core::rand_u64();
                    warn!(stream = format!("{id:016x}"), fenced, "stream writer moves to a new stream");
                    st.id = new;
                    st.next_seq = 0;
                    st.idle_latest = None;
                    if !fenced {
                        // The object may exist: whoever staged in it releases it like any other.
                        st.refs.insert((id, seq), batch.len());
                    }
                    for p in batch {
                        let _ = p.reply.send(Err(match &e {
                            GroupErr::Fenced => GroupErr::Fenced,
                            GroupErr::Changed => GroupErr::Changed,
                            GroupErr::Failed(m, _) => GroupErr::Failed(m.clone(), Some((id, seq))),
                        }));
                    }
                }
            }
        }
    }

    /// Put-if-absent, retrying errors that may be transient with the same body.
    async fn put(&self, id: u64, seq: u64, body: Bytes) -> Result<(), GroupErr> {
        let path = object_path(&self.root, id, seq);
        let mut last = String::new();
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(20 << attempt)).await;
            }
            self.requests.put();
            let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
            match self.store.put_opts(&path, body.clone().into(), opts).await {
                Ok(_) => return Ok(()),
                Err(object_store::Error::AlreadyExists { .. }) => {
                    // Ours, from an attempt that landed after all? Then it's done.
                    self.requests.get();
                    return match self.store.get_range(&path, 0..STREAM_HEADER as u64).await {
                        Ok(h) if h.len() >= 20 && h[0..4] == RECORDS_MAGIC.to_be_bytes() && h[12..20] == id.to_be_bytes() => Ok(()),
                        Ok(_) => Err(GroupErr::Fenced),
                        Err(e) => Err(GroupErr::Failed(e.to_string(), None)),
                    };
                }
                Err(e) => last = e.to_string(),
            }
        }
        Err(GroupErr::Failed(last, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_roundtrip() {
        let recs = [
            Record { key: "q/a".into(), epoch: 3, writer: 9, first: 10, entries: vec![Bytes::from_static(b"x"), Bytes::from_static(b"yz")] },
            Record { key: "q/b".into(), epoch: 1, writer: 2, first: 0, entries: vec![Bytes::new()] },
        ];
        let mut b = BytesMut::new();
        header(&mut b, RECORDS_MAGIC, 7, 7, recs.len() as u32);
        recs.iter().for_each(|r| r.encode(&mut b));
        assert_eq!(b.len(), STREAM_HEADER + recs.iter().map(Record::encoded_len).sum::<usize>());
        let (nonce, back) = decode_object(b.freeze()).unwrap();
        assert_eq!(nonce, 7);
        assert_eq!(back.len(), 2);
        assert_eq!((back[0].key.as_str(), back[0].epoch, back[0].writer, back[0].first), ("q/a", 3, 9, 10));
        assert_eq!(back[0].entries[1], Bytes::from_static(b"yz"));
        assert_eq!(back[1].entries, vec![Bytes::new()]);
        let (_, none) = decode_object(encode_fence(7, 1)).unwrap();
        assert!(none.is_empty());
        assert!(decode_object(Bytes::from_static(b"short")).is_err());
    }
}
