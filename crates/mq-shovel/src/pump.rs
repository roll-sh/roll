//! Moving messages from a source session to a destination session with RabbitMQ's
//! acknowledgement modes. With `on-confirm` a message is acknowledged at the source only after
//! the destination confirmed it, so an outage on either side loses nothing: the session is torn
//! down and the source requeues whatever was still unacknowledged.

use crate::endpoint::{InMsg, Session};
use crate::status::LinkStatus;
use anyhow::bail;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use mq_core::message::Message;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// `ack-mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckMode {
    OnConfirm,
    OnPublish,
    NoAck,
}

impl AckMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on-confirm" => Some(Self::OnConfirm),
            "on-publish" => Some(Self::OnPublish),
            "no-ack" => Some(Self::NoAck),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnConfirm => "on-confirm",
            Self::OnPublish => "on-publish",
            Self::NoAck => "no-ack",
        }
    }
}

/// Where a routed message goes at the destination.
pub struct Out {
    pub msg: Message,
    /// Deliver straight into this queue instead of publishing to `msg.exchange`.
    pub queue: Option<String>,
}

pub struct Pump<'a> {
    pub src: &'a mut Session,
    pub dst: &'a mut Session,
    pub status: &'a LinkStatus,
    pub ack: AckMode,
    /// Stop once this many messages (in total, across reconnects) have been transferred.
    pub limit: Option<u64>,
    /// Consume only while this returns true (queue federation).
    pub gate: Option<Box<dyn FnMut() -> bool + Send + 'a>>,
    /// Source queue whose length is reported as `remaining`.
    pub src_queue: Option<String>,
}

type Pending = FuturesUnordered<BoxFuture<'static, (u64, anyhow::Result<bool>)>>;

impl Pump<'_> {
    /// Transfer until the limit is reached (`Ok`) or an endpoint fails (`Err`). `route` maps a
    /// source message to its destination, or `None` to drop (and acknowledge) it.
    pub async fn run(mut self, mut route: impl FnMut(InMsg) -> Option<Out>) -> anyhow::Result<()> {
        let mut pending: Pending = FuturesUnordered::new();
        let mut published = self.status.forwarded();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks = 0u64;
        loop {
            if self.limit.is_some_and(|n| self.status.forwarded() >= n) {
                return Ok(());
            }
            let room = self.limit.is_none_or(|n| published < n);
            tokio::select! {
                m = self.src.next(), if room => {
                    let m = m?;
                    let tag = m.tag;
                    let Some(out) = route(m) else {
                        if self.ack != AckMode::NoAck {
                            self.src.ack(tag).await?;
                        }
                        continue;
                    };
                    let confirm = self.dst.publish(out.msg, out.queue.as_deref()).await?;
                    published += 1;
                    match self.ack {
                        AckMode::OnConfirm => pending.push(Box::pin(async move { (tag, confirm.await) })),
                        AckMode::OnPublish => {
                            self.src.ack(tag).await?;
                            self.status.forwarded.fetch_add(1, Ordering::Relaxed);
                        }
                        AckMode::NoAck => {
                            self.status.forwarded.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Some((tag, r)) = pending.next(), if !pending.is_empty() => {
                    if r? {
                        self.src.ack(tag).await?;
                        self.status.forwarded.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.src.nack(tag, true).await?;
                        published -= 1;
                    }
                }
                reason = self.dst.closed() => bail!("destination: {reason}"),
                _ = tick.tick() => {
                    ticks += 1;
                    if let Some(gate) = self.gate.as_mut() {
                        if gate() { self.src.resume().await? } else { self.src.pause().await? }
                    }
                    if ticks % 20 == 1
                        && let Some(q) = &self.src_queue
                        && let Some(n) = self.src.queue_len(q).await
                    {
                        self.status.remaining.store(n as i64, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}
