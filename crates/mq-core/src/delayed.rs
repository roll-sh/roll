//! `x-delayed-message` exchanges: messages with an `x-delay` header wait here, durably when
//! persistent, then route like the exchange's `x-delayed-type`.

use crate::log::Log;
use crate::message::Message;
use crate::queue::ConfirmTicket;
use crate::vhost::VHost;
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::warn;

#[allow(clippy::large_enum_variant)] // a mailbox: boxing would allocate per message
enum Cmd {
    Schedule { msg: Message, due_ms: u64, confirm: Option<ConfirmTicket> },
    Stop { destroy: bool },
}

pub struct Delayed {
    tx: mpsc::Sender<Cmd>,
    bytes: Arc<AtomicU64>,
    pending_count: Arc<AtomicU64>,
}

struct Actor {
    vhost: Weak<VHost>,
    exchange: String,
    log: Option<Arc<dyn Log>>,
    pending: BTreeMap<(u64, u64), Arc<Message>>,
    /// seq → log index of the add record.
    persisted: HashMap<u64, u64>,
    live_idx: BTreeMap<u64, u64>,
    next_seq: u64,
    bytes: Arc<AtomicU64>,
    pending_count: Arc<AtomicU64>,
}

fn encode_add(seq: u64, due: u64, msg: &Message) -> Bytes {
    let mut b = BytesMut::with_capacity(17 + msg.encoded_len());
    b.put_u8(1);
    b.put_u64(seq);
    b.put_u64(due);
    msg.encode(&mut b);
    b.freeze()
}

fn encode_remove(seqs: &[u64]) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + seqs.len() * 8);
    b.put_u8(2);
    b.put_u32(seqs.len() as u32);
    for s in seqs {
        b.put_u64(*s);
    }
    b.freeze()
}

impl Delayed {
    pub async fn start(vhost: Weak<VHost>, exchange: &str, durable: bool) -> anyhow::Result<Arc<Self>> {
        let vh = vhost.upgrade().ok_or_else(|| anyhow::anyhow!("vhost gone"))?;
        let log = if durable { Some(vh.broker().logs.open(&format!("d/{}/{}", vh.name, exchange), 0).await?) } else { None };
        let bytes = Arc::new(AtomicU64::new(0));
        let pending_count = Arc::new(AtomicU64::new(0));
        let mut actor = Actor {
            vhost,
            exchange: exchange.into(),
            log,
            pending: BTreeMap::new(),
            persisted: HashMap::new(),
            live_idx: BTreeMap::new(),
            next_seq: 0,
            bytes: bytes.clone(),
            pending_count: pending_count.clone(),
        };
        actor.recover().await?;
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(actor.run(rx));
        Ok(Arc::new(Delayed { tx, bytes, pending_count }))
    }

    pub async fn schedule(&self, msg: Message, delay_ms: u64, confirm: Option<ConfirmTicket>) {
        let due_ms = crate::now_ms() + delay_ms;
        let _ = self.tx.send(Cmd::Schedule { msg, due_ms, confirm }).await;
    }

    /// Stop (shutdown or vhost suspend); pending messages stay in the log.
    pub fn stop(&self) {
        let _ = self.tx.try_send(Cmd::Stop { destroy: false });
    }

    /// Stop and delete pending messages (the exchange was deleted).
    pub fn destroy(&self) {
        let _ = self.tx.try_send(Cmd::Stop { destroy: true });
    }

    pub fn is_idle(&self) -> bool {
        self.pending_count.load(Ordering::Relaxed) == 0
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

impl Actor {
    async fn recover(&mut self) -> anyhow::Result<()> {
        let Some(log) = self.log.clone() else { return Ok(()) };
        let (first, next) = log.bounds();
        let mut live: BTreeMap<u64, (u64, Message, u64)> = BTreeMap::new();
        let mut at = first;
        while at < next {
            let batch = log.read(at, 4096, 32 << 20).await?;
            if batch.is_empty() {
                break;
            }
            for (idx, mut data) in batch {
                at = idx + 1;
                use crate::value::{get_u8, get_u32, get_u64};
                match get_u8(&mut data)? {
                    1 => {
                        let seq = get_u64(&mut data)?;
                        let due = get_u64(&mut data)?;
                        live.insert(seq, (due, Message::decode(&mut data)?, idx));
                    }
                    2 => {
                        for _ in 0..get_u32(&mut data)? {
                            live.remove(&get_u64(&mut data)?);
                        }
                    }
                    _ => {}
                }
            }
        }
        for (seq, (due, msg, idx)) in live {
            self.next_seq = self.next_seq.max(seq + 1);
            self.persisted.insert(seq, idx);
            self.live_idx.insert(idx, seq);
            self.pending.insert((due, seq), Arc::new(msg));
        }
        self.update_bytes();
        Ok(())
    }

    fn update_bytes(&self) {
        self.pending_count.store(self.pending.len() as u64, Ordering::Relaxed);
        self.bytes.store(self.log.as_ref().map(|l| l.bytes()).unwrap_or(0), Ordering::Relaxed);
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Cmd>) {
        loop {
            let next_due = self.pending.first_key_value().map(|((d, _), _)| *d);
            let sleep = match next_due {
                Some(d) => Duration::from_millis(d.saturating_sub(crate::now_ms())),
                None => Duration::from_secs(3600),
            };
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Schedule { msg, due_ms, confirm }) => {
                        let mut batch = vec![(msg, due_ms, confirm)];
                        while let Ok(Cmd::Schedule { msg, due_ms, confirm }) = rx.try_recv() {
                            batch.push((msg, due_ms, confirm));
                        }
                        self.add(batch).await;
                    }
                    Some(Cmd::Stop { destroy }) => {
                        if destroy && let Some(log) = self.log.take() {
                            let _ = log.destroy().await;
                        }
                        return;
                    }
                    None => return,
                },
                _ = tokio::time::sleep(sleep) => self.fire().await,
            }
        }
    }

    async fn add(&mut self, batch: Vec<(Message, u64, Option<ConfirmTicket>)>) {
        let mut records = Vec::new();
        let mut seqs = Vec::new();
        let mut confirms = Vec::new();
        for (msg, due, confirm) in batch {
            let seq = self.next_seq;
            self.next_seq += 1;
            if self.log.is_some() && msg.props.persistent() {
                records.push(encode_add(seq, due, &msg));
                seqs.push(seq);
            }
            self.pending.insert((due, seq), Arc::new(msg));
            confirms.extend(confirm);
        }
        let mut ok = true;
        if let Some(log) = &self.log
            && !records.is_empty()
        {
            match log.append(records).await {
                Ok(first) => {
                    for (i, seq) in seqs.into_iter().enumerate() {
                        self.persisted.insert(seq, first + i as u64);
                        self.live_idx.insert(first + i as u64, seq);
                    }
                }
                Err(e) => {
                    warn!(exchange = %self.exchange, error = %e, "delayed log append failed");
                    ok = false;
                }
            }
        }
        for c in confirms {
            c.settle(ok);
        }
        self.update_bytes();
    }

    async fn fire(&mut self) {
        let Some(vh) = self.vhost.upgrade() else { return };
        let Some(ex) = vh.exchange(&self.exchange) else { return };
        let now = crate::now_ms();
        let mut done = Vec::new();
        while let Some(((due, seq), _)) = self.pending.first_key_value() {
            if *due > now {
                break;
            }
            let (due, seq) = (*due, *seq);
            let msg = self.pending.remove(&(due, seq)).unwrap();
            let msg = Arc::try_unwrap(msg).unwrap_or_else(|m| m.with_props(&m.exchange, &m.routing_key, m.props.clone()));
            if let Err(e) = vh.publish_routed(&ex, msg, None).await {
                warn!(exchange = %self.exchange, error = %e, "delayed publish failed");
            }
            if let Some(idx) = self.persisted.remove(&seq) {
                self.live_idx.remove(&idx);
                done.push(seq);
            }
        }
        if let Some(log) = &self.log
            && !done.is_empty()
        {
            let _ = log.append(vec![encode_remove(&done)]).await;
            let (_, next) = log.bounds();
            let oldest = self.live_idx.first_key_value().map(|(i, _)| *i).unwrap_or(next);
            let _ = log.truncate_prefix(oldest).await;
        }
        self.update_bytes();
    }
}
