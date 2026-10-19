//! Partitioned queues (`x-partitions: N`, a roll extension): one logical queue spread over N
//! partition actors, each of which can live on a different node. Publishes are spread by
//! `x-partition-key` / message id (or round-robin), consumers read from every partition, and
//! ordering holds per partition. Delivery tags carry the partition in their low bits.

use super::*;
use crate::message::Message;
use std::sync::Weak;

const BITS: u32 = 10;
const MASK: u64 = (1 << BITS) - 1;

fn enc(seq: u64, part: usize) -> u64 {
    (seq << BITS) | part as u64
}

fn dec(seq: u64) -> (usize, u64) {
    ((seq & MASK) as usize, seq >> BITS)
}

/// Sink wrapper that rewrites deliveries from a partition to name the logical queue.
struct PartSink {
    inner: Arc<dyn ConsumerSink>,
    part: usize,
    me: Weak<QueueHandle>,
}

impl ConsumerSink for PartSink {
    fn has_credit(&self) -> bool {
        self.inner.has_credit()
    }
    fn deliver(&self, mut d: Delivery) {
        let Some(me) = self.me.upgrade() else { return };
        d.queue = me;
        d.seq = enc(d.seq, self.part);
        self.inner.deliver(d);
    }
    fn cancelled(&self, tag: &str) {
        self.inner.cancelled(tag);
    }
    fn activated(&self, tag: &str, active: bool) {
        self.inner.activated(tag, active);
    }
    fn timed_out(&self, tag: &str, timeout_ms: u64) {
        self.inner.timed_out(tag, timeout_ms);
    }
}

fn partition_for(msg: &Message, n: usize, rr: &mut usize) -> usize {
    let key = msg.props.header("x-partition-key").and_then(|v| v.as_str().map(String::from)).or_else(|| msg.props.message_id.clone());
    match key {
        Some(k) => crate::exchange::hash32(k.as_bytes()) as usize % n,
        None => {
            *rr = (*rr + 1) % n;
            *rr
        }
    }
}

/// Start the logical queue over already-running partitions.
pub fn spawn(meta: QueueMeta, kind: QueueType, parts: Vec<QueueRef>) -> QueueRef {
    let (tx, mut rx) = mpsc::channel(4096);
    let shared = Arc::new(QueueShared::default());
    let me = Arc::new(QueueHandle::new(meta, kind, shared.clone(), tx));
    let weak = Arc::downgrade(&me);
    let parts = Arc::new(parts);

    // Aggregate partition counters so readers of `shared` see the whole queue: every 200 ms
    // while they change, then every 30 s (only TTL expiry moves them without a command), and
    // back to fast on the next command, so an idle partitioned queue barely wakes up.
    let poke = Arc::new(tokio::sync::Notify::new());
    {
        let (parts, shared, weak, poke) = (parts.clone(), shared.clone(), weak.clone(), poke.clone());
        tokio::spawn(async move {
            const FAST: std::time::Duration = std::time::Duration::from_millis(200);
            const SLOW: std::time::Duration = std::time::Duration::from_secs(30);
            const SETTLE: u32 = 25;
            let mut unchanged = 0u32;
            let mut last = None;
            loop {
                let slow = unchanged >= SETTLE;
                tokio::select! {
                    _ = tokio::time::sleep(if slow { SLOW } else { FAST }) => {}
                    _ = poke.notified(), if slow => unchanged = 0,
                }
                if weak.strong_count() == 0 || shared.deleted.load(Ordering::Acquire) {
                    return;
                }
                aggregate(&parts, &shared);
                let now = Some(fingerprint(&shared));
                if now == last {
                    unchanged = unchanged.saturating_add(1);
                } else {
                    unchanged = 0;
                    last = now;
                }
            }
        });
    }

    tokio::spawn(async move {
        let mut rr = 0usize;
        let n = parts.len();
        while let Some(cmd) = rx.recv().await {
            poke.notify_one();
            match cmd {
                QueueCmd::Publish { msg, confirm, reply } => {
                    let p = &parts[partition_for(&msg, n, &mut rr)];
                    let _ = p.send(QueueCmd::Publish { msg, confirm, reply }).await;
                }
                QueueCmd::Consume { spec, reply } => {
                    let mut res = Ok(());
                    for (i, p) in parts.iter().enumerate() {
                        let mut s = (*spec).clone();
                        s.sink = Arc::new(PartSink { inner: spec.sink.clone(), part: i, me: weak.clone() });
                        if let Err(e) = p.consume(s).await {
                            for q in parts.iter().take(i) {
                                q.cancel(spec.channel, spec.tag.clone()).await;
                            }
                            res = Err(e);
                            break;
                        }
                    }
                    let _ = reply.send(res);
                }
                QueueCmd::Cancel { channel, tag, reply } => {
                    let mut any = false;
                    for p in parts.iter() {
                        any |= p.cancel(channel, tag.clone()).await;
                    }
                    if let Some(r) = reply {
                        let _ = r.send(any);
                    }
                }
                QueueCmd::Get { channel, no_ack, reply } => {
                    let mut out = Ok(None);
                    for k in 0..n {
                        let i = (rr + k) % n;
                        match parts[i].get(channel, no_ack).await {
                            Ok(Some(mut d)) => {
                                rr = (i + 1) % n;
                                d.seq = enc(d.seq, i);
                                if let Some(me) = weak.upgrade() {
                                    d.queue = me;
                                }
                                d.remaining = parts.iter().map(|p| p.shared.ready.load(Ordering::Relaxed)).sum::<u64>().min(u32::MAX as u64) as u32;
                                out = Ok(Some(d));
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                out = Err(e);
                                break;
                            }
                        }
                    }
                    let _ = reply.send(out);
                }
                QueueCmd::Settle { channel, seqs, outcome } => {
                    for (i, s) in group(seqs, n) {
                        parts[i].settle(channel, s, outcome).await;
                    }
                    // Credit freed by one partition's ack may let any partition deliver.
                    parts.iter().for_each(|p| p.wake());
                }
                QueueCmd::Release { channel, seqs } => {
                    for (i, s) in group(seqs, n) {
                        let _ = parts[i].send(QueueCmd::Release { channel, seqs: s }).await;
                    }
                    parts.iter().for_each(|p| p.wake());
                }
                QueueCmd::Modify { channel, seqs, modification } => {
                    for (i, s) in group(seqs, n) {
                        parts[i].modify(channel, s, (*modification).clone()).await;
                    }
                    parts.iter().for_each(|p| p.wake());
                }
                QueueCmd::ChannelClosed { channel } => {
                    for p in parts.iter() {
                        p.channel_closed(channel).await;
                    }
                }
                QueueCmd::Recover { channel } => {
                    for p in parts.iter() {
                        let _ = p.send(QueueCmd::Recover { channel }).await;
                    }
                }
                QueueCmd::Wake => {
                    if let Some(me) = weak.upgrade() {
                        me.woke();
                    }
                    for p in parts.iter() {
                        p.wake();
                    }
                }
                QueueCmd::Touch => {
                    for p in parts.iter() {
                        let _ = p.send(QueueCmd::Touch).await;
                    }
                }
                QueueCmd::Reconfigure { config } => {
                    for p in parts.iter() {
                        let _ = p.send(QueueCmd::Reconfigure { config: config.clone() }).await;
                    }
                }
                QueueCmd::Purge { reply } => {
                    let mut total = 0;
                    for p in parts.iter() {
                        total += p.purge().await.unwrap_or(0);
                    }
                    let _ = reply.send(total);
                }
                QueueCmd::Delete { if_unused, if_empty, reply } => {
                    aggregate(&parts, &shared);
                    let Some(me) = weak.upgrade() else { break };
                    if if_unused && shared.consumers.load(Ordering::Relaxed) > 0 {
                        let _ = reply.send(Err(AmqpError::precondition(format!("queue '{}' in vhost '{}' in use", me.meta.name, me.meta.vhost))));
                        continue;
                    }
                    if if_empty && shared.messages() > 0 {
                        let _ = reply.send(Err(AmqpError::precondition(format!("queue '{}' in vhost '{}' is not empty", me.meta.name, me.meta.vhost))));
                        continue;
                    }
                    shared.deleted.store(true, Ordering::Release);
                    let mut total = 0;
                    for p in parts.iter() {
                        let (r, rx) = oneshot::channel();
                        if p.send(QueueCmd::Delete { if_unused: false, if_empty: false, reply: r }).await.is_ok() {
                            total += rx.await.ok().and_then(|r| r.ok()).unwrap_or(0);
                        }
                    }
                    let _ = reply.send(Ok(total));
                    break;
                }
                QueueCmd::Lease { max, visibility_ms, reply } => {
                    let mut out = Vec::new();
                    for k in 0..n {
                        if out.len() >= max {
                            break;
                        }
                        let i = (rr + k) % n;
                        for mut l in parts[i].lease(max - out.len(), visibility_ms).await.unwrap_or_default() {
                            l.seq = enc(l.seq, i);
                            out.push(l);
                        }
                    }
                    rr = (rr + 1) % n;
                    let _ = reply.send(out);
                }
                QueueCmd::SettleLease { seq, delivery_count, outcome, reply } => {
                    let (i, s) = dec(seq);
                    let ok = match parts.get(i) {
                        Some(p) => p.settle_lease(s, delivery_count, outcome).await.unwrap_or(false),
                        None => false,
                    };
                    let _ = reply.send(ok);
                }
                QueueCmd::ExtendLease { seq, delivery_count, visibility_ms, reply } => {
                    let (i, s) = dec(seq);
                    let ok = match parts.get(i) {
                        Some(p) => p.extend_lease(s, delivery_count, visibility_ms).await.unwrap_or(false),
                        None => false,
                    };
                    let _ = reply.send(ok);
                }
                QueueCmd::Peek { offset, count, reply } => {
                    let mut out = Vec::new();
                    let mut skip = offset;
                    for (i, p) in parts.iter().enumerate() {
                        if out.len() >= count {
                            break;
                        }
                        let have = p.shared.messages() as usize;
                        if skip >= have {
                            skip -= have;
                            continue;
                        }
                        for mut m in p.peek(skip, count - out.len()).await.unwrap_or_default() {
                            m.seq = enc(m.seq, i);
                            out.push(m);
                        }
                        skip = 0;
                    }
                    let _ = reply.send(out);
                }
                QueueCmd::Consumers { reply } => {
                    let mut seen = std::collections::HashSet::new();
                    let mut out = Vec::new();
                    for p in parts.iter() {
                        for c in p.consumers().await {
                            if seen.insert((c.channel, c.consumer_tag.clone())) {
                                out.push(c);
                            }
                        }
                    }
                    let _ = reply.send(out);
                }
                QueueCmd::Stop { reply } => {
                    for p in parts.iter() {
                        p.stop().await;
                    }
                    let _ = reply.send(());
                    break;
                }
            }
        }
    });
    me
}

fn group(seqs: Vec<u64>, n: usize) -> Vec<(usize, Vec<u64>)> {
    let mut by: Vec<Vec<u64>> = vec![Vec::new(); n];
    for s in seqs {
        let (i, inner) = dec(s);
        if let Some(v) = by.get_mut(i) {
            v.push(inner);
        }
    }
    by.into_iter().enumerate().filter(|(_, v)| !v.is_empty()).collect()
}

/// What changes when a partitioned queue does anything.
fn fingerprint(s: &QueueShared) -> [u64; 8] {
    let l = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    [l(&s.ready), l(&s.unacked), l(&s.published), l(&s.delivered), l(&s.acked), l(&s.got), l(&s.dead_lettered), l(&s.dropped)]
}

fn aggregate(parts: &[QueueRef], shared: &QueueShared) {
    macro_rules! sum {
        ($($f:ident),*) => {$(
            shared.$f.store(parts.iter().map(|p| p.shared.$f.load(Ordering::Relaxed)).sum(), Ordering::Relaxed);
        )*};
    }
    sum!(ready, unacked, ready_bytes, unacked_bytes, published, delivered, acked, redelivered, got, dead_lettered, dropped, log_bytes);
    // Every consumer is registered on every partition.
    shared.consumers.store(parts.first().map(|p| p.shared.consumers.load(Ordering::Relaxed)).unwrap_or(0), Ordering::Relaxed);
    let last = parts.iter().map(|p| p.shared.last_activity_ms.load(Ordering::Relaxed)).max().unwrap_or(0);
    shared.last_activity_ms.store(last, Ordering::Relaxed);
    if shared.ready.load(Ordering::Relaxed) > 0 {
        shared.available.notify_waiters();
    }
}
